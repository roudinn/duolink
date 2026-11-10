"use strict";

const $ = (id) => document.getElementById(id);
const send = (action, fields = {}) => window.ipc.postMessage(JSON.stringify({ action, ...fields }));
const settings = $("settings");
const network = $("network");
const guestCode = $("guest-code");
const NUMBERS = ["width", "height", "fps", "bitrate"];
const FLAGS = ["yuv444", "hdr", "windowed", "stats"];
const STORE = "duolink.guest.v3";

let profile = "latency";
let state;
let toastTimer;

const PROFILES = {
  latency: "Самые быстрые пресеты энкодера и базовый битрейт. Лучший выбор для динамичных игр.",
  balanced: "Те же быстрые пресеты, битрейт ×1.5 — чище картинка при той же задержке, нужен канал побыстрее.",
  quality: "Более тщательное кодирование (+1–3 мс) и битрейт ×2.5. Для неспешных игр и быстрого интернета.",
};
const HOST_VIEW = {
  working: ["Подготовка", "", "Готовим сессию"],
  invite: ["Ожидание", "warn", "Ждём друга"],
  approval: ["Запрос", "warn", "Разрешить подключение?"],
  live: ["В эфире", "ok", "Вы играете вместе"],
  stopping: ["Завершение", "", "Завершаем сессию"],
};
const GUEST_VIEW = {
  working: ["Подключение", "", "Подключаемся"],
  live: ["В эфире", "ok", "Подключено"],
  stopping: ["Завершение", "", "Отключаемся"],
};

const text = (id, value) => { const el = $(id); if (el.textContent !== value) el.textContent = value; };
const show = (id, on) => { $(id).hidden = !on; };
const tone = (el, kind) => { el.classList.remove("ok", "warn", "done", "busy"); if (kind) el.classList.add(kind); };

function statusLine(id, message, error = false) {
  text(id, message);
  show(id, Boolean(message));
  $(id).classList.toggle("error", error);
}

/* ---------- guest preferences ---------- */

function selectProfile(next) {
  profile = PROFILES[next] ? next : "latency";
  document.querySelectorAll("[data-profile]").forEach((b) => {
    const on = b.dataset.profile === profile;
    b.setAttribute("aria-checked", String(on));
    b.tabIndex = on ? 0 : -1;
  });
  text("profile-note", PROFILES[profile]);
}

function options() {
  const out = { profile, codec: $("codec").value };
  for (const id of NUMBERS) out[id] = $(id).value.trim();
  for (const id of FLAGS) out[id] = $(id).checked;
  return out;
}

function save() {
  try { localStorage.setItem(STORE, JSON.stringify(options())); } catch { /* storage unavailable */ }
}

function restore() {
  let saved = null;
  try { saved = JSON.parse(localStorage.getItem(STORE) || "null"); } catch { /* ignore */ }
  if (saved) {
    for (const id of NUMBERS) if (typeof saved[id] === "string") $(id).value = saved[id];
    for (const id of FLAGS) $(id).checked = Boolean(saved[id]);
    if ([...$("codec").options].some((o) => o.value === saved.codec)) $("codec").value = saved.codec;
  }
  selectProfile(saved?.profile ?? "latency");
}

/* ---------- home ---------- */

const selectedNetwork = () => state?.networks.find((n) => n.address === network.value);

function updateNetworks(list) {
  const keep = network.value;
  network.replaceChildren(...list.map((n) => new Option(n.label, n.address)));
  if (list.some((n) => n.address === keep)) network.value = keep;
}

function renderHostButton() {
  const net = selectedNetwork();
  const blocked = net && net.kind !== "loopback" && !state.components.vigem;
  $("host-start").disabled = !net || blocked;
  text("host-start", blocked ? "Сначала установите драйвер геймпада" : "Создать сессию");
}

function renderPrepare(next) {
  const c = next.components;
  const tsUp = next.networks.some((n) => n.kind === "tailscale");
  const working = next.setup.phase !== "idle";
  const rows = [
    ["c-engines", c.sunshine && c.moonlight, working ? "Загрузка" : "Нужно скачать"],
    ["c-vigem", c.vigem, "Нужно установить"],
    ["c-tailscale", c.tailscale && tsUp, c.tailscale ? "Нужно войти" : "Нужно установить"],
    ["c-firewall", c.firewall, "Нужно настроить"],
  ];
  let done = 0;
  for (const [id, ok, pending] of rows) {
    const li = $(id);
    const busy = !ok && working && (id === "c-engines" || (next.fullSetup && !(id === "c-tailscale" && c.tailscale)));
    tone(li, ok ? "done" : busy ? "busy" : "");
    li.querySelector("em").textContent = ok ? "Готово" : pending;
    if (ok) done += 1;
  }
  const needsInstall = !(c.sunshine && c.moonlight && c.vigem && c.tailscale && c.firewall);
  const needsLogin = c.tailscale && !tsUp;
  show("prepare", needsInstall || needsLogin || working || next.setup.error);
  text("prepare-count", `${done} из ${rows.length}`);
  show("install", needsInstall);
  $("install").disabled = next.fullSetup;
  text("install", next.fullSetup ? "Устанавливаем…" : "Установить всё");
  show("login", needsLogin && !working);
  statusLine("setup-line", working || next.setup.error ? next.setup.message : "", next.setup.error);
}

/* ---------- sessions ---------- */

function renderFacts(id, facts) {
  const dl = $(id);
  show(id, facts.length > 0);
  const key = JSON.stringify(facts);
  if (dl.dataset.key === key) return;
  dl.dataset.key = key;
  dl.replaceChildren(...facts.flatMap(([name, value]) => {
    const dt = document.createElement("dt");
    dt.textContent = name;
    const dd = document.createElement("dd");
    dd.textContent = value;
    if (name === "Маршрут") tone(dd, value.startsWith("Прямой") ? "ok" : value.startsWith("Ретранслятор") ? "warn" : "");
    return [dt, dd];
  }));
}

function renderSession(who, view, table) {
  const active = view.phase !== "idle";
  show(`${who}-live`, active);
  if (!active) {
    statusLine(`${who}-result`, view.message, view.error);
    return;
  }
  const [pill, kind, title] = table[view.phase];
  text(`${who}-pill`, pill);
  tone($(`${who}-pill`), kind);
  text(`${who}-title`, title);
  text(`${who}-message`, view.message);
  renderFacts(`${who}-facts`, view.facts);
  $(`${who}-stop`).disabled = view.phase === "stopping";
}

function applyState(next) {
  const prev = state;
  state = next;
  if (!prev || JSON.stringify(prev.networks) !== JSON.stringify(next.networks)) updateNetworks(next.networks);
  renderHostButton();
  renderPrepare(next);

  const hostActive = next.host.phase !== "idle";
  const guestActive = next.guest.phase !== "idle";
  const loopbackHost = hostActive && selectedNetwork()?.kind === "loopback";
  show("live", hostActive || guestActive);
  show("home", !guestActive && (!hostActive || loopbackHost));
  show("host-card", !hostActive);

  renderSession("host", next.host, HOST_VIEW);
  show("invitation", next.host.phase === "invite");
  show("approval", next.host.phase === "approval");
  text("code-box", next.host.code);
  text("peer-name", next.host.peer);
  if (next.host.code !== prev?.host.code) text("copy-button", "Скопировать код");

  renderSession("guest", next.guest, GUEST_VIEW);
  show("guest-keys", next.guest.phase === "live");
  text("guest-stop", next.guest.phase === "live" ? "Отключиться" : "Отменить");
  guestCode.setAttribute("aria-invalid", String(next.guest.codeError));
  if (prev?.guest.phase === "live" && next.guest.phase === "idle") guestCode.value = "";

  const hags = next.components.hags;
  text("hags-note", hags === true ? "Включено — энкодер работает с приоритетом реального времени"
    : hags === false ? "Выключено. Включите на хосте и перезагрузите ПК — меньше задержка кодирования"
    : "Состояние неизвестно");
  text("version-note", `Версия ${next.version}`);

  if (next.host.phase === "approval" && prev?.host.phase !== "approval" && !settings.open) {
    document.querySelector('[data-action="allow"]').focus();
  }
  if (next.host.phase === "invite" && prev?.host.phase !== "invite" && !settings.open) $("copy-button").focus();
  if (next.guest.codeError && !prev?.guest.codeError) guestCode.focus();
}

function toast(message) {
  text("toast", message);
  show("toast", true);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => show("toast", false), 2600);
}

/* ---------- events ---------- */

document.querySelectorAll("[data-profile]").forEach((b) => {
  b.addEventListener("click", () => { selectProfile(b.dataset.profile); save(); });
  b.addEventListener("keydown", (e) => {
    const order = Object.keys(PROFILES);
    const step = { ArrowLeft: -1, ArrowUp: -1, ArrowRight: 1, ArrowDown: 1 }[e.key];
    if (!step) return;
    e.preventDefault();
    const next = order[(order.indexOf(profile) + step + order.length) % order.length];
    selectProfile(next);
    save();
    document.querySelector(`[data-profile="${next}"]`).focus();
  });
});

document.addEventListener("click", (e) => {
  const button = e.target.closest("button[data-action]");
  if (!button) return;
  const { action } = button.dataset;
  if (action === "settings") settings.showModal();
  else if (action === "close_settings") settings.close();
  else if (action === "host_start") send("host_start", { address: network.value });
  else send(action);
});

settings.addEventListener("click", (e) => { if (e.target === settings) settings.close(); });
network.addEventListener("change", renderHostButton);
$("guest-form").addEventListener("submit", (e) => {
  e.preventDefault();
  save();
  send("guest_start", { code: guestCode.value, options: options() });
});
guestCode.addEventListener("input", () => guestCode.removeAttribute("aria-invalid"));
for (const id of [...NUMBERS, "codec"]) $(id).addEventListener("input", save);
for (const id of FLAGS) $(id).addEventListener("change", save);
window.addEventListener("focus", () => { if (state && !settings.open) send("refresh"); });

window.duo = {
  applyState,
  toast,
  copied() {
    text("copy-button", "Скопировано");
    toast("Код скопирован — отправьте его другу");
  },
  pasted(code) {
    guestCode.value = code;
    guestCode.removeAttribute("aria-invalid");
    guestCode.focus();
  },
};

restore();
send("ready");
