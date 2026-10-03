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

// When the window is too narrow and the recording/status group drops below the
// quick settings, it becomes a full-width second strip of the top bar (CSS .wrapped).
function alignTopBar() {
  const quick = $("#quick"), right = $(".topright");
  right.classList.remove("wrapped");
  const q = quick.getBoundingClientRect(), r = right.getBoundingClientRect();
  if (r.top >= q.bottom) right.classList.add("wrapped");
}
{
  const ro = new ResizeObserver(alignTopBar);
  ["header.topbar", "#quick", "#status"].forEach((s) => ro.observe($(s)));
  window.addEventListener("resize", alignTopBar);
}

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
  $("#view-preview").hidden = v !== "preview";
  $("#view-bench").hidden = v !== "bench";
  $("#tabLibrary").classList.toggle("active", v === "library");
  $("#tabSettings").classList.toggle("active", v === "settings");
  $("#tabPreview").classList.toggle("active", v === "preview");
  $("#tabBench").classList.toggle("active", v === "bench");
  if (v === "bench") renderBench();
  if (v === "settings") openSettings(); else stopMemPoll();
  if (v === "preview") startPreview(); else stopPreview();
}
$("#tabLibrary").addEventListener("click", () => showView("library"));
$("#tabSettings").addEventListener("click", () => showView("settings"));
$("#tabPreview").addEventListener("click", () => showView("preview"));
$("#tabBench").addEventListener("click", () => showView("bench"));
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
    items().filter((el) => !saved.includes(el.dataset.tab)).forEach((el) => bar.appendChild(el)); // newer tabs go last
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
  if ($("#confirm").open || document.querySelector("dialog[open]")) return; // dialogs handle their own Esc / Enter
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
  else if (st.bench) text = st.bench;
  else if (st.recording) {
    const mon = (st.monitor || "").replace(/ \(.*/, "");
    const region = st.region && st.region !== "whole display" ? st.region : "";
    text = [`Recording ${st.game || "desktop"}`, mon, region, `${st.fps} fps`].filter(Boolean).join(" · ");
  } else if (held) {
    const left = Math.max(0, Math.round((st.held_until_ms - Date.now()) / 1000));
    text = `${st.game} closed · buffer kept ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")} · ${hotkeyText()} still saves`;
  } else if (st.game && st.region === "waiting for the capture card") text = "Capture card window open · waiting for the picture";
  else if (st.game && st.region) text = `Waiting for ${st.game}'s window`;
  else if (S.cfg?.mode === "games") text = S.cfg.games.some((g) => g.enabled) ? "Waiting for a game" : "Add a game in Settings to start";
  else text = "Idle";
  $("#statusText").textContent = text;
  $("#status").title = st.summary ? `${text}\n${st.summary}` : text;
}
setInterval(() => { if (S.status?.held_until_ms && !S.status.recording) updateStatusText(); }, 1000);

// Top-bar choice: games (automatic or a running one) or a desktop (one per monitor).
// Picking one switches Settings > Games / Desktop and the monitor too.
function renderTarget(st = S.status) {
  if (!st || !S.cfg) return;
  if (!S.monitors) {
    S.monitors = [];
    invoke("list_monitors").then((m) => { S.monitors = m || []; S.targetSig = ""; renderTarget(); }).catch(() => {});
  }
  const choices = st.choices || [];
  const sel = $("#targetSel");
  const desktop = S.cfg.mode === "desktop";
  const sig = [desktop, choices.map((c) => `${c.exe}|${c.name}|${c.clips}`).join(","), S.monitors.map((m) => m.id + m.label).join(",")].join("#");
  if (sig !== S.targetSig) {
    const opts = [`<option value="g:">Games (automatic)</option>`];
    for (const c of choices) opts.push(`<option value="g:${esc(c.exe)}">${esc(c.name)} · ${plural(c.clips, "clip")}</option>`);
    for (const m of S.monitors) opts.push(`<option value="d:${esc(m.id)}">Desktop · ${esc(m.label)}</option>`);
    sel.innerHTML = opts.join("");
    S.targetSig = sig;
  }
  if (document.activeElement === sel) return;
  if (desktop) sel.value = "d:" + (S.cfg.monitor || S.monitors[0]?.id || "");
  else sel.value = choices.length > 1 && st.target ? "g:" + st.target : "g:";
}
$("#targetWrap").title = "What TUFFClip records: your games (it picks the one you've clipped most, or choose a running one; this restarts its buffer) or a whole desktop.";
$("#targetSel").addEventListener("change", (e) => {
  const v = e.target.value;
  e.target.blur();
  if (v.startsWith("d:")) {
    invoke("set_target", { exe: null }).catch(() => {});
    setCfg({ mode: "desktop", monitor: v.slice(2) });
    return;
  }
  const exe = v.slice(2) || null;
  if (S.cfg.mode !== "games") setCfg({ mode: "games" });
  invoke("set_target", { exe }).catch((x) => toast(String(x), true));
});

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

  if (pv.on) previewStatus();
  if (S.view === "bench") renderBenchWhat();
  const favSig = `${st.favorite || ""}|${st.target || ""}`;
  if (favSig !== S.favSig) { S.favSig = favSig; renderFavUi(); }

  const warn = st.warn || "";
  if (warn && warn !== S.warn) toast(warn, true);
  S.warn = warn;
  $("#hotkeyErr").hidden = !warn || S.view !== "settings";
  if (warn) $("#hotkeyErr").textContent = warn;
}

// ------------------------------------------------------------------ preview
// A live view of what is recorded. The capture process only runs while this tab is open and visible.
const pv = { on: false, sig: "", frames: 0, retry: 0 };
const pvSig = (st) => (st?.recording ? [st.game, st.region, st.fps].join("|") : "");

function pvShow(title, sub) {
  $("#pvImg").hidden = true;
  $("#pvEmpty").hidden = false;
  $("#pvEmptyTitle").textContent = title;
  $("#pvEmptySub").textContent = sub || "";
}
function pvInfo() {
  const st = S.status;
  const rec = !!st?.recording;
  $("#pvTitle").textContent = rec ? `Recording ${st.game || "the desktop"}` : "Nothing is being recorded";
  $("#pvMeta").innerHTML = rec
    ? [st.window_title && `<span>Window: ${esc(st.window_title)}</span>`, st.region && `<span>${esc(st.region)}</span>`, `<span>${st.fps} fps</span>`].filter(Boolean).join("")
    : "";
}
async function startPreview() {
  pv.on = true;
  pv.frames = 0;
  pv.sig = pvSig(S.status);
  pvInfo();
  if (!S.status?.recording) {
    pvShow("Nothing is being recorded.", S.cfg?.mode === "games" ? "Start one of your games and its window shows up here." : "Recording starts as soon as FFmpeg is ready.");
    invoke("preview_stop").catch(() => {});
    return;
  }
  pvShow("Connecting…", "");
  try { await invoke("preview_start"); }
  catch (e) { pvShow("Couldn't start the preview.", String(e)); }
}
function stopPreview() {
  if (!pv.on) return;
  pv.on = false;
  clearTimeout(pv.retry);
  $("#pvImg").removeAttribute("src");
  invoke("preview_stop").catch(() => {});
}
// follow the recording: another game or region means a new capture
function previewStatus() {
  pvInfo();
  const sig = pvSig(S.status);
  if (sig !== pv.sig && !document.hidden) startPreview();
}
document.addEventListener("visibilitychange", () => {
  if (S.view !== "preview") return;
  if (document.hidden) { pv.on = false; clearTimeout(pv.retry); invoke("preview_stop").catch(() => {}); }
  else startPreview();
});

// ------------------------------------------------------------------ benchmark
// Recording on vs off while you play. Basic reads GPU/CPU load (no admin); Full also counts the
// game's real frames through a small admin helper (UAC prompt each test). Numbers come from bench.rs.
const BENCH_LEN = { 2: [4, 15], 4: [6, 20], 8: [8, 30] }; // minutes -> [rounds, seconds per half]
const bench = {
  access: store.get("clipr.benchAccess", "basic"),
  len: store.get("clipr.benchLen", 4),
  running: false,
  prog: null,
  hist: store.get("clipr.benchHist", []),
};
if (!BENCH_LEN[bench.len]) bench.len = 4;
if (!Array.isArray(bench.hist)) bench.hist = [];
bench.hist = bench.hist.filter((x) => x?.r?.finished_ms);
const BENCH_KEEP = 300;
bench.gone = store.get("clipr.benchGone", []); // deleted tests (ids)
if (!Array.isArray(bench.gone)) bench.gone = [];
bench.shown = null; // the test shown on the tab (null = the newest)
bench.cmp = null;   // two test ids compared there instead
bench.pick = [];    // ticked in the All tests dialog
bench.q = "";
bench.sort = store.get("clipr.benchSort", { k: "when", d: -1 });
if (!bench.sort?.k) bench.sort = { k: "when", d: -1 };
const findTest = (id) => bench.hist.find((x) => x.r.finished_ms === id)?.r || null;
const shownTest = () => (bench.shown != null && findTest(bench.shown)) || bench.hist[0]?.r || null;

const pctTxt = (x) => `${Math.abs(x) < 10 ? trimNum(Math.abs(x)) : Math.round(Math.abs(x))}%`;
const fpsTxt = (x) => (x == null ? "–" : x < 100 ? trimNum(x) : String(Math.round(x)));

function renderBenchWhat() {
  const st = S.status;
  const el = $("#benchWhat");
  if (bench.running && bench.prog?.game) el.innerHTML = `<b>${esc(bench.prog.game)}</b>${st?.summary ? " · " + esc(st.summary) : ""}`;
  else if (st?.recording && st.game && !String(st.region || "").includes("capture card")) el.innerHTML = `<b>${esc(st.game)}</b> · ${esc(st.summary)}`;
  else el.textContent = "Your game with your current recording settings. Start one of your games; the test measures the game TUFFClip is recording.";
}

function renderBench() {
  setSeg("#benchAccess", "v", bench.access);
  $$("#benchLen button").forEach((b) => b.classList.toggle("active", Number(b.dataset.v) === bench.len));
  $("#benchAccessHint").textContent = bench.access === "full"
    ? "Also counts your game's real frames, for exact FPS and 1% lows. Reading frame timings needs admin rights, so Windows may ask for permission each test (not if your account is already allowed). Only a small helper gets it, and it closes when the test ends. Works with DirectX games; for others it falls back to the Basic readings."
    : "Reads how busy your GPU and CPU are, like Task Manager. No admin needed, but it can only estimate the frame cost.";
  $$("#benchAccess button, #benchLen button").forEach((b) => (b.disabled = bench.running));
  renderBenchWhat();

  $("#benchStart").hidden = bench.running;
  $("#benchCancel").hidden = !bench.running;
  const [rounds, secs] = BENCH_LEN[bench.len];
  $("#benchInfo").textContent = bench.running ? "" : `${rounds} rounds of ${secs} s on and ${secs} s off`;

  const p = bench.prog;
  $("#benchProg").hidden = !bench.running;
  if (bench.running && p) {
    $("#benchBar").style.width = Math.round((p.pct || 0) * 100) + "%";
    const game = p.game || "your game";
    let t;
    if (p.stage === "starting") t = "Starting the frame counter…";
    else if (p.stage === "admin") t = "Waiting for admin permission…";
    else if (p.stage === "waiting") t = `Switch to ${game}. The test starts when it's in front and being recorded.`;
    else if (p.stage === "away") t = `Paused: switch back to ${game}`;
    else if (p.stage === "finishing") t = "Working out the results…";
    else t = [`Round ${p.round} of ${p.rounds}`, `recording ${p.on ? "on" : "off"}`, p.live_fps ? `${fpsTxt(p.live_fps)} fps` : "", `about ${fmtEta(p.secs_left)} left`].filter(Boolean).join(" · ");
    $("#benchProgInfo").textContent = t;
  }
  renderBenchResult();
  renderBenchCount();
}

$("#benchAccess").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b || bench.running) return;
  bench.access = b.dataset.v;
  store.set("clipr.benchAccess", bench.access);
  renderBench();
});
$("#benchLen").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b || bench.running) return;
  bench.len = Number(b.dataset.v);
  store.set("clipr.benchLen", bench.len);
  renderBench();
});
$("#benchStart").addEventListener("click", async () => {
  const [rounds, phase_secs] = BENCH_LEN[bench.len];
  try {
    bench.running = true;
    bench.prog = { stage: bench.access === "full" ? "starting" : "waiting", pct: 0, game: S.status?.game };
    renderBench();
    await invoke("bench_start", { req: { access: bench.access, rounds, phase_secs } });
  } catch (e) {
    bench.running = false;
    renderBench();
    toast(String(e), true);
  }
});
$("#benchCancel").addEventListener("click", () => invoke("bench_cancel").catch(() => {}));

// What the numbers mean, in words. sev: 0 none, 1 very few frames, 2 some, 3 a lot
function benchVerdict(r) {
  const out = { title: "", lines: [], advice: [], sev: 0 };
  const sevOf = (c) => (c < 3 ? 1 : c < 8 ? 2 : 3);
  if (r.frames) {
    const d = r.fps_diff, n = r.fps_noise;
    if (d == null) {
      out.title = "Not enough data";
      out.lines.push("There weren't enough clean seconds to compare. Stay in the game while the test runs.");
      return out;
    }
    const cost = -d;
    if (n == null || cost <= Math.max(n, 1)) {
      out.title = "No measurable difference";
      out.lines.push(n == null
        ? `Recording on and off came out ${pctTxt(d)} apart.`
        : `Recording on and off were ${pctTxt(d)} apart, within this scene's normal ups and downs (give or take ${pctTxt(n)}).`);
    } else {
      out.sev = sevOf(cost);
      out.title = ["", "Recording costs very few frames", "Recording costs some frames", "Recording costs a lot of frames"][out.sev];
      out.lines.push(`About ${pctTxt(cost)} fewer frames with recording on: ${fpsTxt(r.on.fps)} instead of ${fpsTxt(r.off.fps)} fps on average (give or take ${pctTxt(n)}).`);
    }
    const ld = r.low_diff, ln = r.low_noise;
    if (ld != null && -ld > Math.max(ln ?? 0, 2)) {
      out.lines.push(`The 1% lows drop ${pctTxt(ld)} (${fpsTxt(r.on.low1)} instead of ${fpsTxt(r.off.low1)} fps), so you may notice small stutters.`);
      out.sev = Math.max(out.sev, -ld >= 8 ? 2 : 1);
      if (!out.sev || out.title === "No measurable difference") out.title = "Same average, but more stutter";
    }
  } else {
    if (r.access === "full") out.lines.push("TUFFClip couldn't see this game's frames (it may use Vulkan or OpenGL), so these are estimates from GPU load.");
    if (!r.gpu_bound) {
      out.title = "Probably no frames lost";
      out.lines.push(`Your GPU was ${Math.round(r.on.gpu_total)}% busy with recording on, so it had room left for the game. Recording added about ${pctTxt(r.rec_gpu)} of GPU work.`);
      out.lines.push("It can still cause small stutters, which only real frame counts show.");
    } else if (r.game_own_queue) {
      out.title = "Can't tell from GPU load alone";
      out.lines.push(`Your GPU was fully busy and recording added about ${pctTxt(r.rec_gpu)} of GPU work, but this game draws on its own GPU queue, so its share can't be read from the load.`);
    } else {
      const c = r.est_cost ?? 0;
      out.sev = c < 1 ? 0 : sevOf(c);
      out.title = ["Probably costs no frames", "Probably costs very few frames", "Probably costs some frames", "Probably costs a lot of frames"][out.sev];
      out.lines.push(`Your GPU was fully busy, and the game got about ${pctTxt(c)} less of it with recording on. Expect roughly that many fewer frames.`);
    }
    if (r.access === "basic") out.lines.push("Run the Full test for real FPS and 1% lows.");
  }
  if (out.sev >= 2) {
    // A lower resolution or frame rate only saves encoder work. When the encoder isn't busy, the
    // cost is the capture itself (Path of Exile: 1440p and 1080p, 165 and 60 fps all cost ~15%).
    const native = !r.height || r.height >= r.src_h;
    if (r.on.encoder >= 50) {
      out.advice.push(native && r.src_h > 1080
        ? "To win frames back, record at 1080p instead of full resolution (top bar), then test again."
        : "To win frames back, lower the recording resolution or frame rate (top bar), then test again.");
    } else {
      out.advice.push("Most of this is the cost of capturing the picture, not encoding it, so a lower recording resolution or frame rate won't win much back.");
    }
  }
  if (r.on.encoder > 85) out.advice.push(`The video encoder was nearly maxed out (${Math.round(r.on.encoder)}%). Lower the frame rate or resolution, or clips may stutter.`);
  if (r.on.cpu_total > 90) out.advice.push(`Your CPU was ${Math.round(r.on.cpu_total)}% busy with recording on, so recording's CPU work may cost frames too.`);
  return out;
}

// ---- one test, or two side by side, in the result area
function renderBenchResult() {
  const box = $("#benchResult");
  const cmp = bench.cmp && bench.cmp.map(findTest);
  const r = shownTest();
  box.hidden = bench.running || (!r && !cmp);
  if (box.hidden) return;
  if (cmp && cmp[0] && cmp[1]) renderCompare(box, cmp[0], cmp[1]);
  else { bench.cmp = null; renderSingle(box, r); }
}

const whenTxt = (ms) => new Date(ms).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });

function renderSingle(box, r) {
  const v = benchVerdict(r);
  const pts = (a, b) => { const d = Math.round(a - b); return d === 0 ? "same" : `${d > 0 ? "+" : "−"}${Math.abs(d)} pts`; };
  const chg = (d) => (d == null ? "" : `${d < 0 ? "−" : "+"}${pctTxt(d)}`);
  // The columns pool every frame; the test pairs each round's on and off. Show the change
  // between the columns, and say when the paired test can't tell it from noise.
  const diff = (on, off, d, n) => {
    if (on == null || !off) return "";
    const c = chg(((on - off) / off) * 100);
    return d != null && n != null && Math.abs(d) <= Math.max(n, 1) ? `${c}, within noise` : c;
  };
  const pc = (x) => `${Math.round(x)}%`;
  const rows = [];
  if (r.frames) {
    rows.push(["Average FPS", fpsTxt(r.on.fps), fpsTxt(r.off.fps), diff(r.on.fps, r.off.fps, r.fps_diff, r.fps_noise)]);
    rows.push(["1% low FPS", fpsTxt(r.on.low1), fpsTxt(r.off.low1), diff(r.on.low1, r.off.low1, r.low_diff, r.low_noise)]);
  }
  rows.push(["GPU busy", pc(r.on.gpu_total), pc(r.off.gpu_total), pts(r.on.gpu_total, r.off.gpu_total)]);
  rows.push(["Game's GPU use", pc(r.on.gpu_game), pc(r.off.gpu_game), pts(r.on.gpu_game, r.off.gpu_game)]);
  rows.push(["Other GPU work (Windows, capture, apps)", pc(r.on.gpu_other), pc(r.off.gpu_other), pts(r.on.gpu_other, r.off.gpu_other)]);
  rows.push(["Video encoder", pc(r.on.encoder), pc(r.off.encoder), pts(r.on.encoder, r.off.encoder)]);
  rows.push(["CPU busy", pc(r.on.cpu_total), pc(r.off.cpu_total), pts(r.on.cpu_total, r.off.cpu_total)]);
  rows.push(["CPU used by TUFFClip", `${trimNum(r.on.cpu_rec)}%`, `${trimNum(r.off.cpu_rec)}%`, ""]);
  const newest = bench.hist[0]?.r.finished_ms === r.finished_ms;
  const stats = r.frames
    ? `<div class="bench-stats">
        <div class="stat"><span class="label">Average FPS</span><span><b>${fpsTxt(r.on.fps)}</b> recording on</span><span><b>${fpsTxt(r.off.fps)}</b> off</span></div>
        <div class="stat"><span class="label">1% low FPS</span><span><b>${fpsTxt(r.on.low1)}</b> recording on</span><span><b>${fpsTxt(r.off.low1)}</b> off</span></div>
      </div>`
    : "";
  box.innerHTML = `
    <div class="bench-head"><span class="hint">${newest ? "Your latest test" : "Test"} · ${esc(whenTxt(r.finished_ms))}</span>${newest ? "" : `<button class="link" data-act="latest">Show the latest</button>`}</div>
    <div class="bench-verdict${v.sev >= 2 ? " attn" : ""}">
      <h3>${esc(v.title)}</h3>
      ${v.lines.map((l) => `<p class="hint">${esc(l)}</p>`).join("")}
    </div>
    ${stats}
    <table class="bench-table">
      <thead><tr><th></th><th>Recording on</th><th>Recording off</th><th>Difference</th></tr></thead>
      <tbody>${rows.map((x) => `<tr><td>${x[0]}</td><td>${x[1]}</td><td>${x[2]}</td><td>${x[3]}</td></tr>`).join("")}</tbody>
    </table>
    ${v.advice.map((l) => `<p class="hint">${esc(l)}</p>`).join("")}
    <p class="hint">${esc(r.game)} · ${esc(r.settings)} · ${r.access === "full" ? "Full" : "Basic"} test, ${Math.round(r.on.secs)} s on and ${Math.round(r.off.secs)} s off over ${r.rounds} rounds</p>
    <div class="bench-actions">${favBtn(r)}<button class="btn sm danger" data-act="del" data-id="${r.finished_ms}">Delete test</button></div>`;
}

function renderCompare(box, a, b) {
  if (a.finished_ms > b.finished_ms) [a, b] = [b, a]; // A is the older one
  const A = testRow(a), B = testRow(b);
  const short = (x) => `${x.resTxt} · ${x.fpsTxt} · ${x.codecTxt}`;
  const costWords = (x) => {
    if (x.cost == null) return "an unknown amount";
    if (x.r.frames) return x.inNoise ? "no measurable frames" : `about ${pctTxt(x.cost)} of the frames`;
    return `an estimated ${pctTxt(Math.max(0, x.cost))} of the frames`;
  };
  let title;
  const lines = [];
  if (A.cost == null || B.cost == null) {
    title = "Can't compare what recording costs";
    lines.push("One of these tests didn't have enough clean data to work out the cost. The numbers below still compare.");
  } else {
    const noise = Math.max(Math.hypot(A.noise ?? 0, B.noise ?? 0), 1);
    const gap = A.cost - B.cost; // > 0: B loses fewer frames
    title = Math.abs(gap) <= noise ? "About the same cost" : gap > 0 ? "Test B costs fewer frames" : "Test A costs fewer frames";
    lines.push(`Recording cost ${costWords(A)} in test A (${short(A)}) and ${costWords(B)} in test B (${short(B)}).`);
    if (Math.abs(gap) <= noise && Math.abs(gap) >= 0.5) lines.push(`The ${trimNum(Math.abs(gap))}-point gap is within the tests' normal ups and downs (give or take ${trimNum(noise)}).`);
  }
  if (a.frames && b.frames) lines.push(`With recording on, A ran at ${fpsTxt(a.on.fps)} fps and B at ${fpsTxt(b.on.fps)} fps; 1% lows ${fpsTxt(a.on.low1)} and ${fpsTxt(b.on.low1)}.`);
  else lines.push("Only Full tests count real frames, so a Basic test's cost is an estimate from GPU load.");
  if (A.game !== B.game) lines.push("These are different games, so the scenes differ as well as the settings.");

  const pctChg = (x, y) => (x == null || y == null || !x ? "" : (() => { const d = ((y - x) / x) * 100; return Math.abs(d) < 0.05 ? "same" : `${d < 0 ? "−" : "+"}${pctTxt(d)}`; })());
  const ptsChg = (x, y) => { if (x == null || y == null) return ""; const d = Math.round(y - x); return d === 0 ? "same" : `${d > 0 ? "+" : "−"}${Math.abs(d)} pts`; };
  const pc = (x) => (x == null ? "–" : `${Math.round(x)}%`);
  const fpsOf = (r, side, k) => (r.frames ? r[side][k] : null);
  const rows = [
    ["When", esc(whenTxt(a.finished_ms)), esc(whenTxt(b.finished_ms)), ""],
    ["Game", esc(A.game), esc(B.game), ""],
    ["Recorded", `${A.resTxt} · ${A.fpsTxt}`, `${B.resTxt} · ${B.fpsTxt}`, ""],
    ["Encoding", `${A.codecTxt} · ${esc(A.rateTxt)}`, `${B.codecTxt} · ${esc(B.rateTxt)}`, ""],
    ["Capture", A.captureTxt, B.captureTxt, ""],
    ["Test", `${A.testTxt}, ${a.rounds} rounds`, `${B.testTxt}, ${b.rounds} rounds`, ""],
  ];
  const nums = [
    ["Average FPS, recording on", fpsOf(a, "on", "fps"), fpsOf(b, "on", "fps"), "fps"],
    ["Average FPS, recording off", fpsOf(a, "off", "fps"), fpsOf(b, "off", "fps"), "fps"],
    ["1% low FPS, recording on", fpsOf(a, "on", "low1"), fpsOf(b, "on", "low1"), "fps"],
    ["1% low FPS, recording off", fpsOf(a, "off", "low1"), fpsOf(b, "off", "low1"), "fps"],
    ["Frames lost to recording", A.cost, B.cost, "cost"],
    ["GPU busy, recording on", a.on.gpu_total, b.on.gpu_total, "pc"],
    ["Video encoder, recording on", a.on.encoder, b.on.encoder, "pc"],
    ["CPU busy, recording on", a.on.cpu_total, b.on.cpu_total, "pc"],
  ];
  for (const [l, x, y, k] of nums) {
    if (k === "fps") { if (x == null && y == null) continue; rows.push([l, fpsTxt(x), fpsTxt(y), pctChg(x, y)]); }
    else if (k === "cost") rows.push([l, A.costTxt, B.costTxt, ptsChg(x, y)]);
    else rows.push([l, pc(x), pc(y), ptsChg(x, y)]);
  }
  box.innerHTML = `
    <div class="bench-head"><span class="hint">Comparing two tests</span><button class="link" data-act="stopcmp">Stop comparing</button></div>
    <div class="bench-verdict"><h3>${esc(title)}</h3>${lines.map((l) => `<p class="hint">${esc(l)}</p>`).join("")}</div>
    <table class="bench-table cmp">
      <thead><tr><th></th><th>Test A</th><th>Test B</th><th>B against A</th></tr></thead>
      <tbody>${rows.map((x) => `<tr><td>${x[0]}</td><td>${x[1]}</td><td>${x[2]}</td><td>${x[3]}</td></tr>`).join("")}</tbody>
    </table>
    <div class="bench-actions">${favBtn(a, "A")}${favBtn(b, "B")}</div>`;
}

$("#benchResult").addEventListener("click", (e) => {
  const b = e.target.closest("[data-act]");
  if (!b) return;
  const id = Number(b.dataset.id);
  const act = b.dataset.act;
  if (act === "latest") { bench.shown = null; renderBench(); }
  else if (act === "stopcmp") { bench.cmp = null; renderBench(); }
  else if (act === "fav") makeFav(findTest(id));
  else if (act === "unfav") removeFav(findTest(id));
  else if (act === "del") armClick(b, "Click again to delete", () => { deleteTest(id); renderBench(); });
});

// A first click arms a danger button; a second within 3 s acts.
function armClick(btn, armedLabel, act) {
  if (btn.classList.contains("armed")) { act(); return; }
  const label = btn.textContent;
  btn.classList.add("armed");
  btn.textContent = armedLabel;
  setTimeout(() => { if (btn.isConnected) { btn.classList.remove("armed"); btn.textContent = label; } }, 3000);
}

function renderBenchCount() {
  const n = bench.hist.length;
  $("#benchHistWrap").hidden = !n;
  $("#benchCount").textContent = plural(n, "test");
}

function benchDone(r, quiet = false) {
  bench.running = false;
  bench.prog = null;
  if (!bench.gone.includes(r.finished_ms) && !bench.hist.some((x) => x.r.finished_ms === r.finished_ms)) {
    bench.hist.unshift({ r });
    bench.hist = bench.hist.slice(0, BENCH_KEEP);
    store.set("clipr.benchHist", bench.hist);
    bench.shown = null;
    bench.cmp = null;
  }
  renderBench();
  if (benchDlg.open) renderBenchList();
  if (!quiet) toast(`Benchmark finished: ${benchVerdict(r).title.toLowerCase()}`);
}

function deleteTest(id) {
  bench.hist = bench.hist.filter((x) => x.r.finished_ms !== id);
  store.set("clipr.benchHist", bench.hist);
  // bench_state still hands the newest result back when the window reopens: don't re-add it
  bench.gone = [id, ...bench.gone.filter((x) => x !== id)].slice(0, 50);
  store.set("clipr.benchGone", bench.gone);
  if (bench.shown === id) bench.shown = null;
  if (bench.cmp?.includes(id)) bench.cmp = null;
  bench.pick = bench.pick.filter((x) => x !== id);
}

// ---- favorite settings: a game records with a test's settings instead of the normal ones
// What a test recorded with. Tests from before 0.21 only have the summary text and a few numbers.
function testRec(r) {
  if (r.rec) return r.rec;
  const m = (r.settings || "").match(/([\d.,]+)\s*(Mbps|kbps)/i);
  const kbps = m ? Math.round(parseFloat(m[1].replace(/,/g, "")) * (/^m/i.test(m[2]) ? 1000 : 1)) : 30000;
  return {
    fps: r.rec_fps || 0,
    height: r.height || 0,
    codec: /hevc/i.test(r.settings || "") ? "hevc" : "h264",
    bitrate_auto: true,
    bitrate_kbps: kbps,
    capture_method: r.window_capture ? "window" : "display",
  };
}
function favText(f) {
  return [
    f.height ? `${f.height}p` : "native resolution",
    f.fps ? `${f.fps} fps` : "native frame rate",
    f.codec === "hevc" ? "HEVC" : "H.264",
    f.bitrate_auto ? "automatic bitrate" : fmtRate(f.bitrate_kbps),
    f.capture_method === "window" ? "game window" : "whole display",
  ].join(" · ");
}
const sameFav = (a, b) => !!a && !!b && a.fps === b.fps && a.height === b.height && a.codec === b.codec
  && a.capture_method === b.capture_method && a.bitrate_auto === b.bitrate_auto && (a.bitrate_auto || a.bitrate_kbps === b.bitrate_kbps);
function favGame(r) {
  const games = S.cfg?.games || [];
  const exe = (r.exe || "").toLowerCase();
  return (exe && games.find((g) => g.exe.toLowerCase() === exe)) || games.find((g) => g.name === r.game) || null;
}
const isFavTest = (r) => { const g = favGame(r); return !!g?.favorite && sameFav(g.favorite, testRec(r)); };
function setGame(exe, patch) {
  setCfg({ games: S.cfg.games.map((g) => (g.exe === exe ? { ...g, ...patch } : g)) });
  renderFavUi();
  if (S.view === "bench") renderBench();
  if (benchDlg.open) renderBenchList();
}
function makeFav(r) {
  const g = r && favGame(r);
  if (!g) { toast(`Add ${r?.game || "this game"} to your game list first (Settings > Games).`, true); return; }
  const fav = { ...testRec(r) };
  setGame(g.exe, { favorite: fav, use_favorite: true });
  toast(`${g.name} now records with ${favText(fav)}. Switch back in Settings > Games.`);
}
function removeFav(r) {
  const g = r && favGame(r);
  if (g) setGame(g.exe, { favorite: null });
}
function favBtn(r, which = "") {
  const g = favGame(r);
  const id = r.finished_ms;
  const of = which ? `test ${which}'s settings` : "these";
  if (!g) return `<button class="btn sm" disabled title="Add ${esc(r.game)} to the game list in Settings > Games first">Make ${of} ${esc(r.game)}'s favorite</button>`;
  if (isFavTest(r)) return `<button class="btn sm" data-act="unfav" data-id="${id}" title="${esc(g.name)} records with ${which ? `test ${which}'s` : "these"} settings. Click to go back to the normal settings.">★ ${which ? `${which} is` : "These are"} ${esc(g.name)}'s favorite · Remove</button>`;
  return `<button class="btn sm" data-act="fav" data-id="${id}" title="${esc(g.name)} will record with ${esc(favText(testRec(r)))}. Other games keep the normal settings.">Make ${of} ${esc(g.name)}'s favorite</button>`;
}

// The listed game being recorded with its favorite settings right now (if any).
function activeFav() {
  const st = S.status;
  if (!st?.favorite || !st.target || !S.cfg) return null;
  const t = st.target.toLowerCase();
  return S.cfg.games.find((g) => g.exe.toLowerCase() === t && g.use_favorite && g.favorite) || null;
}
// Quick settings and the Capture note follow the favorite while one is in use.
function renderFavUi() {
  if (!S.cfg) return;
  const g = activeFav();
  $("#qFav").hidden = !g;
  if (g) $("#qFav").title = `${g.name} records with its favorite settings. Resolution and frame rate here change them; the rest of your settings stay as they are.`;
  renderHeight();
  renderFps();
  $("#favNote").hidden = !g;
  if (g) $("#favNoteText").innerHTML = `<b>${esc(g.name)}</b> is recording with its favorite settings: ${esc(favText(g.favorite))}. The settings below apply to every other game.`;
}
$("#favNoteBtn").addEventListener("click", () => { const g = activeFav(); if (g) setGame(g.exe, { use_favorite: false }); });
// Quick settings change the favorite while one is in use, else the normal settings.
function quickPatch(p) {
  const g = activeFav();
  return g ? { games: S.cfg.games.map((x) => (x === g ? { ...x, favorite: { ...x.favorite, ...p } } : x)) } : p;
}

function renderFavs() {
  const box = $("#favList");
  if (!box || box.contains(document.activeElement) && document.activeElement.classList.contains("armed")) return;
  const list = S.cfg.games.filter((g) => g.favorite);
  if (!list.length) { box.innerHTML = `<div class="games-empty">No game has favorite settings yet.</div>`; return; }
  box.innerHTML = list.map((g) => `<div class="game fav-row${g.use_favorite ? "" : " off"}" data-exe="${esc(g.exe)}">
      <span class="name">${esc(g.name)}</span>
      <span class="exe" title="${esc(favText(g.favorite))}">${esc(favText(g.favorite))}${g.use_favorite ? "" : " · not in use"}</span>
      <button class="btn sm" data-k="toggle">${g.use_favorite ? "Use normal settings" : "Use favorite"}</button>
      <button class="btn sm danger" data-k="remove">Remove</button>
    </div>`).join("");
}
$("#favList").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  const row = e.target.closest(".fav-row");
  if (!b || !row) return;
  const g = S.cfg.games.find((x) => x.exe === row.dataset.exe);
  if (!g) return;
  if (b.dataset.k === "toggle") setGame(g.exe, { use_favorite: !g.use_favorite });
  else armClick(b, "Click again to remove", () => setGame(g.exe, { favorite: null }));
});

// ---- the "All tests" dialog: search, sort, tick two to compare
const benchDlg = $("#benchDlg");
const MONTHS = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
const pad2 = (n) => String(n).padStart(2, "0");
const isoDay = (d) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;

// One test as table cells, sort keys and searchable fields.
function testRow(r) {
  const d = new Date(r.finished_ms);
  const s = r.settings || "";
  const size = s.match(/(\d+)\s*×\s*(\d+)/);
  const res = size ? Number(size[2]) : (r.height && r.height < r.src_h ? r.height : r.src_h) || 0;
  const rate = s.match(/([\d.,]+)\s*(Mbps|kbps)/i);
  const mbps = rate ? parseFloat(rate[1].replace(/,/g, "")) / (/^k/i.test(rate[2]) ? 1000 : 1) : null;
  const codec = /hevc/i.test(s) ? "hevc" : "h264";
  const noise = r.frames ? r.fps_noise : null;
  let cost = null;
  if (r.frames) cost = r.fps_diff == null ? null : -r.fps_diff;
  else if (r.est_cost != null) cost = r.est_cost;
  else if (!r.gpu_bound) cost = 0;
  const inNoise = cost != null && r.frames && cost <= Math.max(noise ?? 0, 1);
  const costTxt = cost == null ? "–" : r.frames ? (inNoise ? "none" : pctTxt(cost)) : `≈${pctTxt(Math.max(0, cost))}`;
  const row = {
    r, id: r.finished_ms, when: d, iso: isoDay(d),
    game: r.game || "", exe: r.exe || "",
    res, native: !r.height || r.height >= r.src_h, fps: r.rec_fps || 0,
    codec, codecKeys: codec === "hevc" ? "hevc h265 h.265" : "h264 h.264 avc", mbps, capture: r.window_capture ? "window" : "display", test: r.access === "full" ? "full" : "basic",
    on: r.frames ? r.on.fps : null, off: r.frames ? r.off.fps : null,
    lowon: r.frames ? r.on.low1 : null, lowoff: r.frames ? r.off.low1 : null,
    cost, noise, inNoise, costTxt,
    verdict: benchVerdict(r).title, fav: isFavTest(r),
    resTxt: res ? `${res}p` : "?", fpsTxt: `${r.rec_fps || "?"} fps`, codecTxt: codec === "hevc" ? "HEVC" : "H.264",
    rateTxt: rate ? rate[0] : "?", captureTxt: r.window_capture ? "Window" : "Display", testTxt: r.access === "full" ? "Full" : "Basic",
  };
  row.hay = [
    row.game, row.exe, `${res}p`, row.native ? "native" : "", `${row.fps}fps`, `${row.fps} fps`,
    row.codecKeys, row.rateTxt, row.capture, row.test, row.verdict,
    row.iso, row.iso.slice(0, 7), String(d.getFullYear()), MONTHS[d.getMonth()], d.toLocaleDateString(), row.fav ? "favorite favourite" : "",
  ].join(" ").toLowerCase();
  return row;
}

const QUERY_KEYS = {
  game: "game", name: "game", exe: "exe",
  res: "res", resolution: "res", height: "res",
  fps: "fps", rate: "fps", codec: "codec", bitrate: "mbps", mbps: "mbps",
  capture: "capture", method: "capture", test: "test", access: "test", type: "test",
  on: "on", avg: "on", off: "off", low: "lowon", lowon: "lowon", lowoff: "lowoff",
  cost: "cost", lost: "cost", date: "date", when: "date", day: "date", month: "date", year: "date",
  fav: "fav", favorite: "fav", favourite: "fav", result: "verdict", verdict: "verdict",
};
const NUM_KEYS = ["res", "fps", "mbps", "on", "off", "lowon", "lowoff", "cost"];
const unquote = (s) => s.replace(/^"(.*)"$/, "$1");

// "poe 1080p fps>=120 -basic res:1080,1440 game:"path of exile"" -> conditions that must all hold
function parseQuery(q) {
  const toks = q.match(/-?[a-z]+(?:>=|<=|[:<>=])"[^"]*"|-?"[^"]*"|\S+/gi) || [];
  return toks.map((t) => {
    let neg = false;
    if (t.length > 1 && t.startsWith("-") && !/^-\d/.test(t)) { neg = true; t = t.slice(1); }
    const m = t.match(/^([a-z]+)(>=|<=|[:<>=])(.*)$/i);
    const key = m && QUERY_KEYS[m[1].toLowerCase()];
    if (key) return { neg, key, op: m[2] === "=" ? ":" : m[2], vals: unquote(m[3]).toLowerCase().split(",").map((v) => v.trim()).filter(Boolean) };
    return { neg, text: unquote(t).toLowerCase() };
  });
}
const monthOf = (v) => (v.length >= 3 ? MONTHS.findIndex((m) => m.startsWith(v)) : -1);
function dateMatch(op, v, row) {
  v = v.replace(/\//g, "-");
  if (v === "today" || v === "yesterday") {
    const d = new Date();
    if (v === "yesterday") d.setDate(d.getDate() - 1);
    v = isoDay(d);
  }
  const mi = monthOf(v);
  if (mi >= 0) return op === ":" ? row.when.getMonth() === mi : false;
  const p = v.match(/^(\d{4})(?:-(\d{1,2}))?(?:-(\d{1,2}))?$/);
  if (!p) return false;
  const k = [p[1], p[2] && pad2(p[2]), p[3] && pad2(p[3])].filter(Boolean).join("-");
  // a partial date is a whole period: "> 2026-09" means after September
  if (op === ":") return row.iso.startsWith(k);
  if (op === ">=") return row.iso >= k;
  if (op === ">") return row.iso > k + "￿";
  if (op === "<") return row.iso < k;
  return row.iso <= k + "￿";
}
function fieldMatch(key, op, v, row) {
  if (key === "date") return dateMatch(op, v, row);
  if (key === "fav") return /^(n|no|false|0|off)$/.test(v) ? !row.fav : row.fav;
  if (key === "res" && v === "native") return row.native;
  if (NUM_KEYS.includes(key)) {
    const n = parseFloat(v.replace(/[^\d.\-]/g, ""));
    const x = row[key];
    if (!isFinite(n) || x == null) return false;
    if (op === ":") return Math.round(x * 10) / 10 === n || Math.round(x) === Math.round(n);
    return op === ">" ? x > n : op === "<" ? x < n : op === ">=" ? x >= n : x <= n;
  }
  if (key === "codec") return row.codecKeys.includes(v);
  return String(row[key] ?? "").toLowerCase().includes(v);
}
function rowMatches(row, conds) {
  return conds.every((c) => {
    let ok;
    if (c.text != null) {
      const t = c.text;
      ok = t === "today" || t === "yesterday" || /^\d{4}[-/]\d{1,2}/.test(t) ? dateMatch(":", t, row) : row.hay.includes(t);
    } else ok = !c.vals.length || c.vals.some((v) => fieldMatch(c.key, c.op, v, row));
    return c.neg ? !ok : ok;
  });
}

const BENCH_COLS = [
  ["when", "When", -1], ["game", "Game", 1], ["res", "Recorded", -1], ["enc", "Encoding", -1], ["capture", "Capture", 1], ["test", "Test", 1],
  ["on", "Avg FPS on", -1], ["off", "Avg FPS off", -1], ["lowon", "1% low on", -1], ["lowoff", "1% low off", -1], ["cost", "Frames lost", 1],
];
function sortRows(rows) {
  const { k, d } = bench.sort;
  const key = {
    when: (x) => x.id, game: (x) => x.game.toLowerCase(), res: (x) => x.res * 1000 + x.fps,
    enc: (x) => (x.codec === "hevc" ? 1e6 : 0) + (x.mbps || 0), capture: (x) => x.capture, test: (x) => x.test,
  }[k] || ((x) => x[k]);
  return rows.sort((a, b) => {
    const x = key(a), y = key(b);
    if (x == null || y == null) return x == null ? (y == null ? b.id - a.id : 1) : -1; // empty cells last
    return (x < y ? -1 : x > y ? 1 : b.id - a.id) * (x === y ? 1 : d);
  });
}

function renderBenchList() {
  const all = bench.hist.map((x) => testRow(x.r));
  const conds = parseQuery(bench.q);
  const rows = sortRows(all.filter((r) => rowMatches(r, conds)));
  $("#benchDlgCount").textContent = rows.length === all.length ? plural(all.length, "test") : `${rows.length} of ${all.length}`;
  const shown = shownTest()?.finished_ms;
  const arrow = (k) => (bench.sort.k === k ? `<span class="sort">${bench.sort.d > 0 ? "▲" : "▼"}</span>` : "");
  const head = `<thead><tr><th class="ck"></th>${BENCH_COLS.map(([k, l]) => `<th data-sort="${k}" class="${bench.sort.k === k ? "sorted" : ""}">${l}${arrow(k)}</th>`).join("")}<th class="act"></th></tr></thead>`;
  const fv = (x) => (x == null ? `<span class="dim">–</span>` : fpsTxt(x));
  const body = rows.map((x) => {
    const g = favGame(x.r);
    const fav = !g ? `<button class="btn sm ghost" disabled title="Add ${esc(x.game)} to the game list in Settings > Games first">Favorite</button>`
      : x.fav ? `<button class="btn sm ghost on" data-act="unfav" title="${esc(g.name)} records with these settings. Click to go back to the normal settings.">★ Favorite</button>`
      : `<button class="btn sm ghost" data-act="fav" title="Make these ${esc(g.name)}'s favorite settings">Favorite</button>`;
    const costTitle = x.r.frames ? (x.inNoise ? `Within this test's normal ups and downs (give or take ${pctTxt(x.noise ?? 1)})` : "Fewer frames with recording on") : "Estimated from GPU load (Basic test)";
    return `<tr data-id="${x.id}" class="${x.id === shown && !bench.cmp ? "shown" : ""}" title="${esc(x.verdict)}">
      <td class="ck"><input type="checkbox" data-pick ${bench.pick.includes(x.id) ? "checked" : ""} aria-label="Pick for comparing"></td>
      <td>${esc(x.when.toLocaleDateString([], { dateStyle: "medium" }))} <span class="dim">${esc(x.when.toLocaleTimeString([], { timeStyle: "short" }))}</span></td>
      <td class="g">${esc(x.game)}</td>
      <td>${x.resTxt} · ${x.fpsTxt}</td>
      <td>${x.codecTxt} · ${esc(x.rateTxt)}</td>
      <td>${x.captureTxt}</td>
      <td>${x.testTxt}</td>
      <td>${fv(x.on)}</td><td>${fv(x.off)}</td><td>${fv(x.lowon)}</td><td>${fv(x.lowoff)}</td>
      <td title="${esc(costTitle)}">${x.costTxt}</td>
      <td class="act">${fav}<button class="btn sm ghost danger" data-act="del">Delete</button></td>
    </tr>`;
  }).join("");
  const empty = rows.length ? "" : `<tr><td colspan="${BENCH_COLS.length + 2}" class="none">${all.length ? "No test matches that search." : "No tests yet."}</td></tr>`;
  $("#benchList").innerHTML = head + `<tbody>${body}${empty}</tbody>`;
  renderPick();
}
function renderPick() {
  bench.pick = bench.pick.filter((id) => findTest(id));
  const n = bench.pick.length;
  $("#benchPickHint").textContent = n === 0 ? "Tick two tests to compare them. Click a row to show that test on the Benchmark tab."
    : n === 1 ? "Tick one more test to compare." : "Two tests ticked.";
  $("#benchCompare").disabled = n !== 2;
  $("#benchPickClear").hidden = !n;
}

$("#benchAll").addEventListener("click", () => {
  bench.pick = bench.cmp ? [...bench.cmp] : [];
  renderBenchList();
  benchDlg.showModal();
  $("#benchSearch").focus();
});
$("#benchSearch").addEventListener("input", (e) => {
  bench.q = e.target.value;
  renderBenchList();
});
$("#benchCompare").addEventListener("click", () => {
  if (bench.pick.length !== 2) return;
  bench.cmp = [...bench.pick];
  benchDlg.close();
  renderBench();
  $("#benchResult").scrollIntoView({ block: "start", behavior: "smooth" });
});
$("#benchPickClear").addEventListener("click", () => { bench.pick = []; renderBenchList(); });
benchDlg.addEventListener("click", (e) => {
  if (e.target.dataset.close !== undefined || e.target === benchDlg) { benchDlg.close(); return; }
  const th = e.target.closest("th[data-sort]");
  if (th) {
    const k = th.dataset.sort;
    bench.sort = bench.sort.k === k ? { k, d: -bench.sort.d } : { k, d: BENCH_COLS.find((c) => c[0] === k)[2] };
    store.set("clipr.benchSort", bench.sort);
    renderBenchList();
    return;
  }
  const tr = e.target.closest("tr[data-id]");
  if (!tr) return;
  const id = Number(tr.dataset.id);
  if (e.target.matches("[data-pick]")) {
    bench.pick = e.target.checked ? [...bench.pick.filter((x) => x !== id), id].slice(-2) : bench.pick.filter((x) => x !== id);
    $$("#benchList [data-pick]").forEach((c) => (c.checked = bench.pick.includes(Number(c.closest("tr").dataset.id))));
    renderPick();
    return;
  }
  const b = e.target.closest("button");
  if (b) {
    if (b.dataset.act === "fav") makeFav(findTest(id));
    else if (b.dataset.act === "unfav") removeFav(findTest(id));
    else if (b.dataset.act === "del") armClick(b, "Sure?", () => { deleteTest(id); renderBenchList(); renderBench(); });
    return;
  }
  if (e.target.closest("td.ck")) return;
  bench.shown = id;
  bench.cmp = null;
  benchDlg.close();
  renderBench();
});

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
  video.removeAttribute("src");
  S.dur = 0; S.start = 0; S.end = 0;
  disarmDelete();
  renderList();
  renderTrim();
  const path = clip.path;
  loadSound(path);
  invoke("probe_clip", { path }).then((c) => {
    if (!S.sel || S.sel.path !== path) return;
    S.srcCodec = c;
    renderExport();
    loadVideo(path, c);
  }).catch(() => { if (S.sel && S.sel.path === path) loadVideo(path, ""); });
}

// The embedded WebView2 often can't decode HEVC (black picture), so those clips play from an
// H.264 copy made on demand; the file itself is untouched.
async function loadVideo(path, codec) {
  // The video's sound can only go through Web Audio (for the track levels) when it was fetched
  // with CORS. If that fails, the error handler loads it again without.
  if (video.dataset.noCors !== "1") video.crossOrigin = "anonymous";
  if (codec !== "hevc") { video.src = convertFileSrc(path); return; }
  toast("Preparing a preview of this HEVC clip…");
  try {
    const proxy = await invoke("playback_proxy", { path });
    if (S.sel && S.sel.path === path) video.src = convertFileSrc(proxy);
  } catch (e) {
    toast(String(e), true);
    if (S.sel && S.sel.path === path) video.src = convertFileSrc(path);
  }
}

function closeClip() {
  clearSound();
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
  const src = video.getAttribute("src");
  if (src && video.crossOrigin && !snd.routed) {
    video.dataset.noCors = "1";
    video.removeAttribute("crossorigin");
    video.src = src;
    return;
  }
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
    moveLevels(S.sel.path, np);
    S.sel = { ...S.sel, path: np, name: stemOf(np) };
    title.value = S.sel.name;
    toast("Renamed");
  } catch (e) {
    toast(String(e), true);
    title.value = S.sel.name;
  }
  loadVideo(S.sel.path, S.srcCodec);
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
  if (snd.tracks.length) drawWaves();
  renderExport();
}
function renderHead() {
  $("#tlHead").style.left = pct(video.currentTime || 0) + "%";
  renderSoundHead();
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

// ------------------------------------------------------------------ sound tracks
// WebView2 only plays a video's first sound track, so each track plays from its own decoded
// copy, through a Web Audio gain, in step with the video. The video's own sound is silenced
// while they play; its volume and mute buttons still work as the master volume.
const snd = { ctx: null, master: null, vgain: null, routed: false, tracks: [], path: "", hasMix: false };
const LEVELS_KEY = "clipr.levels";

function audioCtx() {
  if (!snd.ctx) {
    snd.ctx = new AudioContext();
    snd.master = snd.ctx.createGain();
    snd.master.connect(snd.ctx.destination);
    applyMaster();
  }
  return snd.ctx;
}
function applyMaster() {
  if (snd.master) snd.master.gain.value = video.muted ? 0 : video.volume;
}
// The video's own sound goes through the graph too, so the master volume covers it and it
// can be silenced while the tracks play. Done once: an element can't be routed twice.
function routeVideo() {
  if (snd.routed || video.crossOrigin !== "anonymous") return;
  try {
    const ctx = audioCtx();
    snd.vgain = ctx.createGain();
    ctx.createMediaElementSource(video).connect(snd.vgain).connect(snd.master);
    snd.routed = true;
  } catch {}
}

const levelsOf = (path) => store.get(LEVELS_KEY, {})[path] || {};
function saveLevels() {
  if (!snd.path) return;
  const all = store.get(LEVELS_KEY, {});
  const mine = {};
  for (const t of snd.tracks) if (t.gain !== 1 || t.muted) mine[t.index] = { gain: t.gain, muted: t.muted };
  delete all[snd.path];
  if (Object.keys(mine).length) all[snd.path] = mine; // re-added last: newest at the end
  const keys = Object.keys(all);
  for (const k of keys.slice(0, Math.max(0, keys.length - 300))) delete all[k]; // keep the newest 300 clips
  store.set(LEVELS_KEY, all);
}
function moveLevels(from, to) {
  const all = store.get(LEVELS_KEY, {});
  if (all[from]) { if (to) all[to] = all[from]; delete all[from]; store.set(LEVELS_KEY, all); }
  if (snd.path === from) snd.path = to || "";
}

// What the export does with the sound: null = the clip's first track as it is.
const levelsChanged = () => snd.tracks.some((t) => t.gain !== 1 || t.muted);
function exportLevels() {
  // Several tracks but no stored mix (shouldn't happen with TUFFClip's clips): mix them anyway.
  if (!levelsChanged() && !(snd.tracks.length > 1 && !snd.hasMix)) return null;
  return snd.tracks.map((t) => ({ index: t.index, gain: t.muted ? 0 : t.gain }));
}

function clearSound() {
  for (const t of snd.tracks) {
    t.el.pause();
    t.el.removeAttribute("src");
    t.el.load();
    try { t.node.disconnect(); } catch {}
  }
  snd.tracks = [];
  snd.path = "";
  if (snd.vgain) snd.vgain.gain.value = 1;
  $("#sound").hidden = true;
  $("#tracks").innerHTML = "";
}

async function loadSound(path) {
  clearSound();
  snd.path = path;
  $("#sound").hidden = false;
  $("#resetLevels").hidden = true;
  $("#soundHint").textContent = "Reading the sound tracks…";
  let list;
  try {
    list = await invoke("clip_audio", { path });
  } catch (e) {
    if (snd.path === path) $("#soundHint").textContent = "Couldn't read this clip's sound tracks, so their levels can't be changed.";
    return;
  }
  if (snd.path !== path || !S.sel || S.sel.path !== path) return;
  if (!list.length) { $("#sound").hidden = true; return; }
  // Clips with several tracks start with a mix of them all; you edit the parts instead.
  const parts = list.filter((t) => t.title !== "Mix");
  const shown = list.length > 1 && parts.length ? parts : list;
  snd.hasMix = shown.length < list.length;
  // Waveforms are drawn relative to the clip's loudest track (up to 4x), so quiet clips still show.
  const loudest = Math.max(1, ...shown.map((t) => Math.max(0, ...t.peaks)));
  snd.scale = Math.min(4, 255 / loudest);
  const saved = levelsOf(path);
  const ctx = audioCtx();
  routeVideo();
  snd.tracks = shown.map((t) => {
    const el = new Audio();
    el.crossOrigin = "anonymous";
    el.preload = "auto";
    el.src = convertFileSrc(t.file);
    const node = ctx.createGain();
    ctx.createMediaElementSource(el).connect(node).connect(snd.master);
    const lv = saved[t.index] || {};
    const tr = { index: t.index, name: t.title || "Sound", peaks: t.peaks, el, node, gain: lv.gain ?? 1, muted: !!lv.muted };
    node.gain.value = tr.muted ? 0 : tr.gain;
    return tr;
  });
  // If the video's own sound couldn't be routed, it can't be silenced: keep it and leave the tracks quiet.
  if (snd.routed) snd.vgain.gain.value = 0;
  else for (const t of snd.tracks) t.node.disconnect();
  $("#soundHint").textContent = !snd.routed
    ? "Levels can't be heard here, but exports use them."
    : snd.tracks.length > 1 ? "Exports mix the tracks into one at these levels." : "Exports use this level.";
  renderTracks();
  syncSound(true);
  renderExport();
}

function renderTracks() {
  $("#tracks").innerHTML = snd.tracks.map((t, i) => `
    <div class="track${t.muted ? " muted" : ""}" data-i="${i}">
      <div class="wave"><canvas></canvas><span class="track-name">${esc(t.name)}</span><div class="wave-head"></div></div>
      <button class="btn sm ghost mute" aria-pressed="${t.muted}">${t.muted ? "Muted" : "Mute"}</button>
      <input type="range" min="0" max="200" step="5" value="${Math.round(t.gain * 100)}" aria-label="${esc(t.name)} level" title="Double-click for 100%">
      <span class="lvl">${Math.round(t.gain * 100)}%</span>
    </div>`).join("");
  $("#resetLevels").hidden = !levelsChanged();
  drawWaves();
  renderSoundHead();
}
function setLevel(i, patch) {
  const t = snd.tracks[i];
  Object.assign(t, patch);
  t.node.gain.value = t.muted ? 0 : t.gain;
  const row = $(`#tracks .track[data-i="${i}"]`);
  row.classList.toggle("muted", t.muted);
  row.querySelector(".mute").textContent = t.muted ? "Muted" : "Mute";
  row.querySelector(".mute").setAttribute("aria-pressed", t.muted);
  row.querySelector("input").value = Math.round(t.gain * 100);
  row.querySelector(".lvl").textContent = `${Math.round(t.gain * 100)}%`;
  $("#resetLevels").hidden = !levelsChanged();
  drawWave(row, t);
  saveLevels();
  renderExport();
}
$("#tracks").addEventListener("input", (e) => {
  const row = e.target.closest(".track");
  if (row && e.target.type === "range") setLevel(Number(row.dataset.i), { gain: Number(e.target.value) / 100 });
});
$("#tracks").addEventListener("dblclick", (e) => {
  const row = e.target.closest(".track");
  if (row && e.target.type === "range") setLevel(Number(row.dataset.i), { gain: 1 });
});
$("#tracks").addEventListener("click", (e) => {
  const row = e.target.closest(".track");
  if (row && e.target.closest(".mute")) setLevel(Number(row.dataset.i), { muted: !snd.tracks[Number(row.dataset.i)].muted });
});
// a click on a waveform seeks, like the timeline
$("#tracks").addEventListener("pointerdown", (e) => {
  const wave = e.target.closest(".wave");
  if (!wave || !S.dur) return;
  const r = wave.getBoundingClientRect();
  video.currentTime = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)) * S.dur;
  renderHead();
});
$("#resetLevels").addEventListener("click", () => snd.tracks.forEach((_, i) => setLevel(i, { gain: 1, muted: false })));

// Loudness across the whole clip, dimmed outside the trim, scaled by the track's level.
function drawWave(row, t) {
  const cv = row.querySelector("canvas");
  const w = cv.clientWidth, h = cv.clientHeight;
  const n = t.peaks.length;
  if (!w || !h || !n) return;
  const dpr = window.devicePixelRatio || 1;
  cv.width = Math.round(w * dpr);
  cv.height = Math.round(h * dpr);
  const g = cv.getContext("2d");
  g.scale(dpr, dpr);
  const css = getComputedStyle(document.documentElement);
  const inside = css.getPropertyValue("--muted").trim(), outside = css.getPropertyValue("--idle").trim();
  const dur = S.dur || n / 20;
  const level = t.muted ? 0 : Math.sqrt(t.gain); // peaks are square-root scaled
  const mid = h / 2;
  for (let x = 0; x < w; x += 2) {
    const a = Math.floor((x / w) * dur * 20);
    const b = Math.max(a + 1, Math.floor(((x + 2) / w) * dur * 20));
    let p = 0;
    for (let k = a; k < b && k < n; k++) p = Math.max(p, t.peaks[k]);
    const v = Math.min(1, (p / 255) * level * (snd.scale || 1)) * (h / 2 - 3);
    const at = (x / w) * dur;
    g.fillStyle = at >= S.start && at <= S.end ? inside : outside;
    g.fillRect(x, mid - v - 0.5, 1.5, v * 2 + 1);
  }
}
function drawWaves() {
  $$("#tracks .track").forEach((row) => drawWave(row, snd.tracks[Number(row.dataset.i)]));
}
function renderSoundHead() {
  const left = pct(video.currentTime || 0) + "%";
  $$("#tracks .wave-head").forEach((el) => (el.style.left = left));
}
window.addEventListener("resize", () => { if (snd.tracks.length) drawWaves(); });

// keep the tracks in step with the video
function syncSound(force = false) {
  if (!snd.tracks.length) return;
  const t = video.currentTime || 0;
  for (const tr of snd.tracks) {
    const el = tr.el;
    if (force || Math.abs(el.currentTime - t) > 0.08) { try { el.currentTime = t; } catch {} }
    el.playbackRate = video.playbackRate;
    if (video.paused || video.seeking) { if (!el.paused) el.pause(); }
    else if (el.paused) el.play().catch(() => {});
  }
}
video.addEventListener("play", () => { snd.ctx?.resume(); syncSound(true); });
video.addEventListener("pause", () => syncSound(true));
video.addEventListener("seeking", () => syncSound(true));
video.addEventListener("seeked", () => syncSound(true));
video.addEventListener("waiting", () => snd.tracks.forEach((t) => t.el.pause()));
video.addEventListener("playing", () => syncSound(true));
video.addEventListener("ratechange", () => syncSound());
video.addEventListener("timeupdate", () => syncSound());
video.addEventListener("volumechange", applyMaster);

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
        ? "Re-encodes as HEVC at the same picture quality. It takes longer, and not every player or site takes HEVC."
        : "Re-encodes as H.264 at the same picture quality. It takes longer, but plays everywhere.";
      hint += levelsChanged() ? " The sound is mixed at your levels." : " The sound is untouched.";
    } else {
      est = `About <b>${fmtMb(n.mb * (S.format === "mkv" ? 0.99 : 1))}</b>`;
      hint = "Cuts land on the nearest keyframe. Instant and lossless." +
        (S.format === "mkv" ? " .mkv comes out about 1% smaller than .mp4." : "") +
        (levelsChanged() ? " Only the sound is re-encoded, to mix it at your levels." : "");
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
        levels: exportLevels(),
      },
    });
    toast(`${isPng() ? "Saved" : "Exported"} ${baseName(out)}`);
    S.nameTouched = false;
    loadClips(); // the raw clip now shows as exported
    invoke("reveal_clip", { path: out }).catch(() => {});
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
    moveLevels(path, null);
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
  const canScale = !S.ff || S.ff.gpu_scale !== false;
  const fav = activeFav()?.favorite;
  const opts = [];
  let cur = S.cfg.height, q = fav ? fav.height : cur;
  if (m) {
    opts.push([0, `${m.width}×${m.height} (native)`]);
    const hs = [1440, 1080, 720, 480];
    for (const h of [cur, q]) if (h && h < m.height && !hs.includes(h)) hs.push(h);
    hs.filter((h) => h < m.height).sort((a, b) => b - a)
      .forEach((h) => opts.push([h, `${even((m.width * h) / m.height)}×${h}`, !canScale]));
    // as big as the display is the same as no scaling
    if (cur >= m.height) cur = 0;
    if (q >= m.height) q = 0;
  } else {
    opts.push([0, "Native"], [1440, "1440p"], [1080, "1080p"], [720, "720p"]);
  }
  fillSelect($("#height"), opts, cur);
  fillSelect($("#qHeight"), opts, q);
}

function renderFps() {
  const hz = monitorHz();
  const fav = activeFav()?.favorite;
  const opts = [[0, hz ? `${hz} fps (native)` : "Native"]];
  const list = [30, 60, 120, 144, 165, 240];
  for (const f of [hz, S.cfg.fps, fav?.fps]) if (f && !list.includes(f)) list.push(f);
  list.sort((a, b) => a - b).filter((f) => !hz || f <= hz).forEach((f) => opts.push([f, `${f} fps`]));
  const cap = (f) => (hz && f > hz ? hz : f);
  fillSelect($("#fps"), opts, cap(S.cfg.fps));
  fillSelect($("#qFps"), opts, cap(fav ? fav.fps : S.cfg.fps));
  $("#fpsHint").textContent = !S.cfg.fps
    ? "Follows your display's refresh rate."
    : hz && S.cfg.fps > hz
      ? `Your display only runs at ${hz} Hz, so recording is capped there.`
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
    ? "The X closes this window; TUFFClip keeps recording from the tray."
    : "The X quits TUFFClip completely, which also stops recording.";
  set("#monitorSel", (el) => { if (c.monitor) el.value = c.monitor; });
  set("#captureName", (el) => (el.value = c.capture_name));
  $("#cardHint").textContent = c.mode === "games"
    ? "The window shows the card's picture and plays its sound so you can play on it. While it's open, TUFFClip records it at the card's own resolution (fullscreen looks sharpest) and your clip shortcut saves from it."
    : "The window shows the card's picture and plays its sound so you can play on it. You record a monitor all the time, so put the window on that monitor; clips made while it's focused are filed under the name below.";
  set("#drawMouse", (el) => (el.checked = c.draw_mouse));
  set("#indicator", (el) => (el.value = c.indicator));
  set("#startHidden", (el) => (el.checked = c.start_hidden));
  set("#closeToTray", (el) => (el.checked = c.close_to_tray));
  set("#minToTray", (el) => (el.checked = c.minimize_to_tray));
  set("#beep", (el) => (el.checked = c.beep));
  set("#checkUpdates", (el) => (el.checked = c.check_updates));
  $("#updRow").hidden = !c.check_updates;
  set("#audio", (el) => (el.checked = c.audio));
  renderAudio(c);
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
    : `For these settings TUFFClip would pick ${fmtRate(autoRate(c))}.`;
  renderFps();
  renderHeight();
  renderGames();
  renderFavs();
  renderFavUi();
  renderIgnored();
  $("#hkKey").textContent = c.hotkey;
  set("#hkDlgInput", (el) => { if (!el.classList.contains("listening")) el.value = c.hotkey; });
  renderTarget();
  $("#customRateUnit").textContent = unitLabel();
}

// Settings > Audio: which tracks get recorded
let mics = [];
function renderAudio(c = S.cfg) {
  const desktopMode = c.mode === "desktop";
  const game = c.audio_source === "game" && !desktopMode;
  setSeg("#audioSource", "v", c.audio_source);
  $("#audioSourceWrap").hidden = !c.audio;
  $("#audioSourceHint").textContent = desktopMode
    ? "You record a monitor, not a game, so the main track is always everything you hear."
    : game
      ? "Only the game's own sound: music, videos and other apps stay out of your clips."
      : "Everything your PC plays, on one track, including Discord and music.";
  $("#discordWrap").hidden = !c.audio || !game;
  $("#micWrap").hidden = !c.audio;
  $("#micDeviceWrap").hidden = !c.audio || !c.mic;
  $("#tracksHint").hidden = !c.audio || !((game && c.discord_track) || c.mic); // only one track: nothing to mix
  $("#discordTrack").checked = c.discord_track;
  $("#mic").checked = c.mic;
  const missing = c.mic_device && !mics.some((m) => m.id === c.mic_device);
  const opts = [["", "Windows default"], ...mics.map((m) => [m.id, m.name])];
  if (missing) opts.push([c.mic_device, "Not plugged in"]);
  fillSelect($("#micDevice"), opts, c.mic_device || "");
  $("#micHint").textContent = missing
    ? "That microphone isn't plugged in, so the Windows default is recorded until it's back."
    : "Recorded as it comes in, without Discord's noise filtering or push-to-talk.";
  $("#micHint").classList.toggle("err", !!missing);
}
async function loadMics() {
  try { mics = await invoke("list_mics"); } catch { mics = []; }
  if (S.cfg) renderAudio();
}

async function openSettings() {
  loadMics();
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
    $("#aboutName").textContent = `TUFFClip ${info.version}`;
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

onChange("#captureName", (el) => {
  const name = el.value.trim();
  if (!name) { el.value = S.cfg.capture_name; return; }
  if (name === S.cfg.capture_name) return;
  if (folderName(name) !== folderName(S.cfg.capture_name)) S.renamedFrom.push(S.cfg.capture_name);
  return { capture_name: name };
});
const openCapture = () => invoke("open_capture").catch((e) => toast(String(e), true));
$("#openCapture").addEventListener("click", openCapture);
$("#openCapture2").addEventListener("click", openCapture);
onChange("#drawMouse", (el) => ({ draw_mouse: el.checked }));
onChange("#indicator", (el) => ({ indicator: el.value }));
onChange("#startHidden", (el) => ({ start_hidden: el.checked }));
onChange("#closeToTray", (el) => ({ close_to_tray: el.checked }));
onChange("#minToTray", (el) => ({ minimize_to_tray: el.checked }));
onChange("#beep", (el) => ({ beep: el.checked }));
onChange("#checkUpdates", (el) => {
  if (el.checked) setTimeout(() => checkUpdates(false), 600); // after the setting is saved
  else { upd.rel = null; upd.msg = ""; renderUpdate(); }
  return { check_updates: el.checked };
});
onChange("#audio", (el) => ({ audio: el.checked }));
onChange("#discordTrack", (el) => ({ discord_track: el.checked }));
onChange("#mic", (el) => ({ mic: el.checked }));
onChange("#micDevice", (el) => ({ mic_device: el.value }));
$$("#audioSource button").forEach((b) => b.addEventListener("click", () => setCfg({ audio_source: b.dataset.v })));
onChange("#gentleSave", (el) => ({ gentle_save: el.checked }));
onChange("#bufferRam", (el) => ({ buffer_in_ram: el.checked }));
onChange("#height", (el) => ({ height: Number(el.value) }));
onChange("#qHeight", (el) => quickPatch({ height: Number(el.value) }));
onChange("#fps", (el) => ({ fps: Number(el.value) }));
onChange("#qFps", (el) => quickPatch({ fps: Number(el.value) }));
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
onChange("#clipsDir", (el) => {
  if (!el.value.trim()) return;
  setTimeout(() => loadClips().catch(() => {}), 800); // after the setting is saved
  return { clips_dir: el.value.trim() };
});
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
// popups: the lists live in dialogs, with a search box that hides rows that don't match
function filterRows(box, q) {
  q = q.trim().toLowerCase();
  let shown = 0;
  box.querySelectorAll(".game").forEach((r) => {
    const hit = !q || r.textContent.toLowerCase().includes(q) || [...r.querySelectorAll("input.name")].some((i) => i.value.toLowerCase().includes(q));
    r.hidden = !hit;
    if (hit) shown++;
  });
  box.querySelector(".no-match")?.remove();
  if (q && !shown && box.querySelector(".game")) box.insertAdjacentHTML("beforeend", `<div class="games-empty no-match">Nothing matches.</div>`);
}
function setupListDialog(openBtn, dlg, search, box) {
  $(openBtn).addEventListener("click", () => {
    $(search).value = "";
    filterRows($(box), "");
    $(dlg).showModal();
  });
  $(search).addEventListener("input", (e) => filterRows($(box), e.target.value));
  $(dlg).addEventListener("click", (e) => { if (e.target.dataset.close !== undefined || e.target === $(dlg)) $(dlg).close(); });
}
setupListDialog("#openGames", "#gamesDlg", "#gamesSearch", "#gameList");
setupListDialog("#openIgnored", "#ignoredDlg", "#ignoredSearch", "#ignoredList");

function renderGames() {
  const box = $("#gameList");
  $("#gamesCount").textContent = `${S.cfg.games.length} listed`;
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
        <select data-k="indicator" aria-label="Recording indicator for this game" title="Recording indicator for this game">
          ${[["", "Indicator: default"], ["off", "Indicator: off"], ["top_left", "Top left"], ["top_right", "Top right"], ["bottom_left", "Bottom left"], ["bottom_right", "Bottom right"]]
            .map(([v, l]) => `<option value="${v}"${(g.indicator || "") === v ? " selected" : ""}>${l}</option>`).join("")}
        </select>
        <button class="btn sm ghost" data-k="remove">Remove</button>
      </div>`
    )
    .join("");
  filterRows(box, $("#gamesSearch").value);
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
  if (!row) return;
  const g = S.cfg.games[Number(row.dataset.i)];
  if (e.target.dataset.k === "indicator") {
    g.indicator = e.target.value || null;
    schedule(150);
    return;
  }
  if (e.target.dataset.k !== "name") return;
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
  S.cfg.games.push({ exe, name: exe.replace(/\.exe$/i, ""), enabled: true, indicator: null });
  renderGames();
  schedule(150);
}

// ---- hidden programs: kept out of the running-apps list until you show them again
function renderIgnored() {
  const box = $("#ignoredList");
  const list = S.cfg.ignored_exes || [];
  box.innerHTML = list.length
    ? list.map((x, i) => `<div class="game ignored" data-i="${i}"><span class="exe" title="${esc(x)}">${esc(x)}</span><button class="btn sm ghost" data-k="unhide">Show again</button></div>`).join("")
    : `<div class="games-empty">Nothing hidden.</div>`;
  $("#ignoredCount").textContent = `${list.length} hidden`;
  filterRows(box, $("#ignoredSearch").value);
}
$("#ignoredList").addEventListener("click", async (e) => {
  if (e.target.dataset.k !== "unhide") return;
  S.cfg.ignored_exes.splice(Number(e.target.closest(".game").dataset.i), 1);
  renderIgnored();
  await flush();
  refreshRunning();
});
$("#hideRunning").addEventListener("click", async () => {
  const sel = $("#runningSel");
  if (!sel.value) return;
  S.cfg.ignored_exes = [...(S.cfg.ignored_exes || []), sel.value];
  sel.value = "";
  renderIgnored();
  await flush();
  refreshRunning();
});

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
function bindHotkey(el, key, optional, errEl = $("#hotkeyErr"), done = null) {
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
      errEl.textContent = "That's already your other shortcut. Pick a different one.";
      errEl.hidden = false;
      return;
    }
    errEl.hidden = true;
    if (combo !== S.cfg[key]) {
      S.cfg[key] = combo;
      flush(); // a taken shortcut is reported (and reverted) straight away
    }
    done?.();
  });
}
bindHotkey($("#hotkey"), "hotkey", false);
bindHotkey($("#hotkey2"), "hotkey2", true);
bindHotkey($("#hkDlgInput"), "hotkey", false, $("#hkDlgErr"), () => $("#hkDlg").close());
$("#hkRemind").addEventListener("click", () => {
  $("#hkDlgErr").hidden = true;
  $("#hkDlgInput").value = S.cfg.hotkey;
  $("#hkDlg").showModal();
  $("#hkDlgInput").focus();
});
$("#hkDlg").addEventListener("click", (e) => { if (e.target.dataset.close !== undefined || e.target === $("#hkDlg")) $("#hkDlg").close(); });

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
  const old = !!f.ok && !(f.ddagrab && f.gfxcapture);
  $("#getFfmpeg").hidden = !!f.ok && !old;
  $("#getFfmpeg").textContent = old ? "Update FFmpeg" : "Download FFmpeg";
  $("#ffBanner").hidden = !!f.ok && !old;
  $("#ffBannerBtn").hidden = !!f.ok && !old;
  $("#ffBannerBtn").textContent = old ? "Update FFmpeg" : "Download FFmpeg";
  if (old) $("#ffBannerText").textContent = "This FFmpeg is too old for TUFFClip (no window capture). Updating downloads a current one and keeps your old copy untouched.";
  if (!f.ok) {
    $("#ffBannerText").textContent = "FFmpeg isn't set up yet. TUFFClip needs it to record and export.";
    $("#ffHint").textContent = "FFmpeg wasn't found. Use the button above to download it, or enter the full path to ffmpeg.exe.";
    $("#encHint").textContent = "";
    return;
  }
  $("#ffHint").textContent = !f.ddagrab
    ? `${f.version}. This build has no ddagrab filter, so screen capture won't work. Use a full build from gyan.dev or BtbN.`
    : !f.gfxcapture
      ? `${f.version}. No gfxcapture filter (needs FFmpeg 8 full), so games are recorded from the display instead. Use "Download FFmpeg" for a current build.`
      : f.version;
  $("#encHint").textContent = f.encoders.length
    ? `Your FFmpeg supports: ${f.encoders.map((e) => ({ nvenc: "NVIDIA", amf: "AMD", qsv: "Intel" }[e])).join(", ")}`
    : "This FFmpeg build has no hardware encoders.";
  $("#scaleHint").textContent = f.gpu_scale ? "" : "Native only: this FFmpeg can't resize on the GPU (needs FFmpeg 8 with gfxcapture).";
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
    : "No raw clips have a trimmed version yet. TUFFClip remembers which ones you've exported.";
}
$("#pickClips").addEventListener("click", async () => {
  const dir = await invoke("pick_folder", { title: "Choose where TUFFClip saves clips", start: S.cfg.clips_dir }).catch((e) => { toast(String(e), true); return null; });
  if (!dir || dir === S.cfg.clips_dir) return;
  setCfg({ clips_dir: dir });
  await flush();
  toast(`New clips go to ${dir}.`);
  loadClips().catch(() => {});
});
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
      `TUFFClip and ffmpeg use <b>${fmtSize(m.tuffclip_bytes)}</b>` +
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

// ---- updates (only while Settings > About > Check for updates is on)
const upd = { rel: null, busy: false, later: false, msg: "" };
function renderUpdate() {
  const r = upd.rel;
  $("#updBanner").hidden = !(upd.busy || (r && !upd.later && S.cfg?.check_updates));
  $("#updBannerBtn").hidden = upd.busy;
  $("#updBannerLater").hidden = upd.busy;
  $("#updInstall").hidden = !r || upd.busy;
  $("#updCheckNow").disabled = upd.busy;
  if (r && !upd.busy) {
    upd.msg = `TUFFClip ${r.version} is available.`;
    $("#updBannerText").textContent = upd.msg;
  }
  $("#updHint").textContent = upd.msg;
}
async function checkUpdates(force) {
  if (!S.cfg?.check_updates || upd.busy) return;
  if (force) { upd.msg = "Checking…"; renderUpdate(); }
  try {
    upd.rel = await invoke("update_check", { force });
    upd.msg = upd.rel ? "" : "You have the newest version.";
  } catch (e) {
    upd.rel = null;
    upd.msg = String(e);
    if (force) toast(String(e), true);
  }
  renderUpdate();
}
async function installUpdate() {
  const r = upd.rel;
  if (!r || upd.busy) return;
  if (S.exporting) { toast("Wait for the export to finish, then update.", true); return; }
  $("#updDlgTitle").textContent = `Update to TUFFClip ${r.version}?`;
  $("#updDlgBody").textContent = `TUFFClip downloads it from GitHub (${fmtSize(r.size)}), closes, and opens the new version. The clip buffer starts over, so save anything you want to keep first.`;
  const dlg = $("#updDlg");
  dlg.returnValue = "";
  dlg.showModal();
  await new Promise((ok) => dlg.addEventListener("close", ok, { once: true }));
  if (dlg.returnValue !== "ok") return;
  upd.busy = true;
  upd.later = false;
  const say = (t) => { upd.msg = t; $("#updBannerText").textContent = t; renderUpdate(); };
  say(`Downloading TUFFClip ${r.version}…`);
  const un = await listen("update-download", (e) =>
    say(`Downloading TUFFClip ${r.version}… ${fmtSize(e.payload)} of ${fmtSize(r.size)}`));
  try {
    await invoke("update_install", { rel: r });
    say("Restarting…");
  } catch (e) {
    un();
    upd.busy = false;
    say(String(e));
    toast(String(e), true);
    // TUFFClip can't replace itself in this folder: hand over to the download page.
    if (String(e).includes("write to its own folder")) invoke("open_releases_page").catch(() => {});
  }
}
$("#updBannerBtn").addEventListener("click", installUpdate);
$("#updInstall").addEventListener("click", installUpdate);
$("#updBannerLater").addEventListener("click", () => { upd.later = true; renderUpdate(); });
$("#updCheckNow").addEventListener("click", () => checkUpdates(true));

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
    if (info.updated) toast(`TUFFClip was updated to ${info.version}`);
    if (info.config_broken) toast("Your settings file was damaged, so TUFFClip started with defaults. The old file is saved as config.broken.json.", true);
  } catch {}

  checkUpdates(false); // at most every 15 minutes, and only if turned on
  listen("update-available", (e) => {
    if (upd.busy || !S.cfg?.check_updates) return;
    upd.rel = e.payload;
    upd.msg = "";
    renderUpdate();
  });
  listen("status", (e) => renderStatus(e.payload));
  listen("clip-saved", (e) => {
    toast(`Saved ${baseName(e.payload)}`);
    if (S.kind === "raw") loadClips();
  });
  listen("clip-error", (e) => toast(String(e.payload), true));
  listen("preview-frame", (e) => {
    if (!pv.on) return;
    if (!pv.frames++) { $("#pvEmpty").hidden = true; $("#pvImg").hidden = false; }
    $("#pvImg").src = "data:image/jpeg;base64," + e.payload;
  });
  listen("preview-ended", () => {
    if (!pv.on) return;
    if (!pv.frames) pvShow("No picture from this window.", "If the clips come out black too, try Whole display under Settings, Capture, Capture method.");
    clearTimeout(pv.retry);
    pv.retry = setTimeout(() => { if (pv.on && S.status?.recording && !document.hidden) startPreview(); }, 3000);
  });
  listen("bench-progress", (e) => { bench.running = true; bench.prog = e.payload; renderBench(); });
  listen("bench-done", (e) => benchDone(e.payload));
  listen("bench-error", (e) => {
    bench.running = false;
    bench.prog = null;
    renderBench();
    toast(e.payload.msg, !e.payload.cancelled);
  });
  try {
    const b = await invoke("bench_state");
    bench.running = b.running;
    bench.prog = b.progress;
    if (b.result && !bench.hist.some((x) => x.r.finished_ms === b.result.finished_ms)) benchDone(b.result, true);
  } catch {}

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

