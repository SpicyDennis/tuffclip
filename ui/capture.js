// Capture card window: shows the card's live picture and plays its sound, so you can play on it.
// While this window is open, TUFFClip records it like a game (see Engine::capture_hwnd).
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s) => document.querySelector(s);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

const KEY_VIDEO = "clipr.captureVideo";
const KEY_AUDIO = "clipr.captureAudio";
const KEY_VOLUME = "clipr.captureVolume";
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch {} },
};

// Names capture cards tend to have (USB3.0 Capture, HDMI Capture, Cam Link, ...). Webcams rarely match.
const CARD = /captur|hdmi|usb ?3|uvc|cam link|elgato|guermok|macrosilicon|ms21\d\d|grabber|video ?in/i;
const MIC = /microphone|headset|webcam|array/i;

const feed = $("#feed");
let stream = null;
let starting = false;
let hotkey = "the clip shortcut";
let mode = "games";

// ------------------------------------------------------------------ picture and sound
function showEmpty(title, sub = "", retry = false, label = "Try again") {
  $("#capEmpty").hidden = false;
  $("#capTitle").textContent = title;
  $("#capSub").textContent = sub;
  $("#capRetry").hidden = !retry;
  $("#capRetry").textContent = label;
}

function stopStream() {
  stream?.getTracks().forEach((t) => { t.onended = null; t.stop(); });
  stream = null;
  feed.srcObject = null;
  invoke("capture_feed", {}).catch(() => {});
}

async function devices() {
  const all = await navigator.mediaDevices.enumerateDevices();
  return { video: all.filter((d) => d.kind === "videoinput"), audio: all.filter((d) => d.kind === "audioinput") };
}

function pickVideo(list) {
  const saved = store.get(KEY_VIDEO);
  return list.find((d) => d.deviceId === saved) || list.find((d) => CARD.test(d.label)) || list[0];
}

// The card's own sound input: same physical device as the picture. Never a microphone by guess,
// because playing a mic through the speakers would feed back.
function pickAudio(list, video) {
  const saved = store.get(KEY_AUDIO);
  if (saved === "none") return null;
  const chosen = list.find((d) => d.deviceId === saved);
  if (chosen) return chosen;
  const core = (video?.label || "").replace(/\s*\(.*\)\s*$/, "").trim();
  return (
    list.find((a) => video?.groupId && a.groupId === video.groupId && a.deviceId !== "default" && a.deviceId !== "communications") ||
    (core.length > 3 && list.find((a) => a.label.toLowerCase().includes(core.toLowerCase()))) ||
    list.find((a) => CARD.test(a.label) && !MIC.test(a.label)) ||
    null
  );
}

function fillSelects(video, audio, v, a) {
  $("#videoSel").innerHTML = video.map((d, i) => `<option value="${esc(d.deviceId)}">${esc(d.label || `Camera ${i + 1}`)}</option>`).join("");
  if (v) $("#videoSel").value = v.deviceId;
  $("#audioSel").innerHTML =
    `<option value="none">No sound</option>` +
    audio
      .filter((d) => d.deviceId !== "default" && d.deviceId !== "communications")
      .map((d, i) => `<option value="${esc(d.deviceId)}">${esc(d.label || `Input ${i + 1}`)}</option>`)
      .join("");
  $("#audioSel").value = a ? a.deviceId : "none";
}

function reportFeed() {
  const t = stream?.getVideoTracks()[0];
  if (!t || t.readyState !== "live") return;
  const s = t.getSettings();
  const width = feed.videoWidth || s.width;
  const height = feed.videoHeight || s.height;
  if (!width || !height) return;
  invoke("capture_feed", { width, height, fps: Math.round(s.frameRate || 60) }).catch(() => {});
}

async function open(v, a) {
  const audio = a
    ? { deviceId: { exact: a.deviceId }, echoCancellation: false, noiseSuppression: false, autoGainControl: false, channelCount: { ideal: 2 }, sampleRate: { ideal: 48000 }, latency: { ideal: 0 } }
    : false;
  const video = { deviceId: { exact: v.deviceId }, width: { ideal: 1920 }, height: { ideal: 1080 }, frameRate: { ideal: 60 } };
  try {
    return await navigator.mediaDevices.getUserMedia({ video, audio });
  } catch (e) {
    if (e.name !== "OverconstrainedError") throw e;
    return navigator.mediaDevices.getUserMedia({ video: { deviceId: { exact: v.deviceId } }, audio });
  }
}

async function start() {
  if (starting) return;
  starting = true;
  stopStream();
  try {
    // Windows sees capture cards as cameras. TUFFClip never asks for camera access by itself:
    // until it is allowed here, it only offers to look for the card.
    const cfg = await invoke("get_config");
    if (!cfg.camera_access) {
      showEmpty("Look for your capture card?", "Windows treats capture cards as cameras, so TUFFClip needs camera access to see yours. Only this window uses it.", true, "Look for capture card");
      return;
    }
    showEmpty("Looking for your capture card…");
    let { video, audio } = await devices();
    // Device names are only readable once camera access is allowed; ask once, then look again.
    if (!video.length || video.some((d) => !d.label)) {
      try {
        const probe = await navigator.mediaDevices.getUserMedia({ video: true });
        probe.getTracks().forEach((t) => t.stop());
      } catch (e) {
        if (e.name !== "NotFoundError") throw e;
      }
      ({ video, audio } = await devices());
    }
    if (!video.length) {
      fillSelects([], audio, null, null);
      showEmpty("No capture card found.", "Plug the card into a USB 3 port (usually blue) and connect your Switch's dock to its HDMI input. The picture shows up here by itself.");
      return;
    }
    const v = pickVideo(video);
    const a = pickAudio(audio, v);
    fillSelects(video, audio, v, a);

    stream = await open(v, a);
    const track = stream.getVideoTracks()[0];
    track.onended = () => {
      stopStream();
      showEmpty("The capture card was disconnected.", "Plug it back in and the picture comes back by itself.", true);
    };
    feed.srcObject = stream;
    feed.volume = volume();
    await feed.play().catch(() => {});
    $("#capEmpty").hidden = true;
    reportFeed();
  } catch (e) {
    stopStream();
    if (e.name === "NotAllowedError") {
      showEmpty("TUFFClip isn't allowed to use the capture card.", "Windows blocks camera access for desktop apps. Turn on Settings > Privacy > Camera > Let desktop apps access your camera, then try again.", true);
    } else if (e.name === "NotReadableError" || e.name === "AbortError") {
      showEmpty("The capture card is busy.", "Another program (OBS, Discord, the Camera app) is using it. Close that program, then try again.", true);
    } else {
      showEmpty("Couldn't open the capture card.", String(e.message || e), true);
    }
  } finally {
    starting = false;
  }
}

feed.addEventListener("resize", reportFeed);
$("#capRetry").addEventListener("click", async () => {
  try {
    const cfg = await invoke("get_config");
    if (!cfg.camera_access) await invoke("set_camera_access", { on: true });
  } catch {}
  start();
});

let changeTimer = 0;
navigator.mediaDevices.addEventListener("devicechange", () => {
  clearTimeout(changeTimer);
  // Plugging in a card fires several of these; react once it settles.
  changeTimer = setTimeout(() => { if (!stream || !stream.active) start(); }, 1200);
});

$("#videoSel").addEventListener("change", (e) => {
  store.set(KEY_VIDEO, e.target.value);
  store.set(KEY_AUDIO, ""); // a different card has its own sound
  e.target.blur();
  start();
});
$("#audioSel").addEventListener("change", (e) => {
  store.set(KEY_AUDIO, e.target.value);
  e.target.blur();
  start();
});

function volume() {
  const v = parseFloat(store.get(KEY_VOLUME));
  return isFinite(v) ? Math.min(1, Math.max(0, v)) : 1;
}
$("#volume").value = volume();
$("#volume").addEventListener("input", (e) => {
  feed.volume = Number(e.target.value);
  store.set(KEY_VOLUME, e.target.value);
});

// ------------------------------------------------------------------ recording status
function renderStatus(st) {
  const el = $("#capStatus");
  const card = st.target === "<capture card>";
  el.classList.toggle("live", !!st.recording);
  el.classList.toggle("err", !!st.error);
  let text;
  if (st.error) text = st.error;
  else if (st.recording && card) text = `Recording ${st.game} · ${hotkey} saves a clip`;
  else if (st.recording) text = mode === "desktop" ? `Recording the monitor · ${hotkey} saves a clip` : `Recording ${st.game}`;
  else if (card) text = "Waiting for the picture";
  else text = "Not recording";
  $("#capStatusText").textContent = text;
  el.title = st.summary ? `${text}\n${st.summary}` : text;
}

(async () => {
  try {
    const cfg = await invoke("get_config");
    hotkey = cfg.hotkey || hotkey;
    mode = cfg.mode;
  } catch {}
  try { renderStatus(await invoke("get_status")); } catch {}
  listen("status", (e) => renderStatus(e.payload));
})();

// ------------------------------------------------------------------ fullscreen and controls
async function fullscreen(on) {
  try {
    const now = await invoke("set_fullscreen", on === undefined ? {} : { on });
    $("#fsBtn").firstChild.textContent = now ? "Exit fullscreen" : "Fullscreen";
  } catch {}
}
$("#fsBtn").addEventListener("click", () => fullscreen());
feed.addEventListener("dblclick", () => fullscreen());
document.addEventListener("keydown", (e) => {
  if (e.key === "F11" || (e.altKey && e.key === "Enter")) { e.preventDefault(); fullscreen(); }
  else if (e.key === "Escape") fullscreen(false);
});

// The bar hides after a moment without mouse movement, so it doesn't sit over the game (or the recording).
let idleTimer = 0;
function wake() {
  document.body.classList.remove("idle");
  clearTimeout(idleTimer);
  idleTimer = setTimeout(() => {
    if ($("#capBar").matches(":hover")) { wake(); return; }
    if ($("#capEmpty").hidden) document.body.classList.add("idle");
  }, 2500);
}
document.addEventListener("mousemove", wake);
document.addEventListener("mousedown", wake);
wake();

window.addEventListener("beforeunload", stopStream);
start();
