"use strict";

const $ = (id) => document.getElementById(id);
const send = (action, fields = {}) => window.ipc.postMessage(JSON.stringify({ action, ...fields }));
const setup = $("setup-dialog");
const network = $("network");
const guestCode = $("guest-code");
const numbers = ["width", "height", "fps", "bitrate"];
const flags = ["yuv444", "hdr", "windowed", "stats"];
const STORE = "duolink.guest.v2";

let role = "host";
let profile = "balanced";
let state;
let toastTimer;

const PROFILE_NOTES = {
  latency: "Самые быстрые пресеты энкодера и базовый битрейт. Для слабого интернета и соревновательных игр.",
  balanced: "Быстрые пресеты и битрейт ×1.5. Задержка как у «Задержки», картинка заметно чище.",
  quality: "Медленные пресеты энкодера (+1–3 мс) и битрейт ×2.5. Нужен быстрый канал у хоста.",
};

const HOST_STATES = {
  working: ["Подготовка", ""],
  invite: ["Ожидание", "warn"],
  approval: ["Запрос", "warn"],
  live: ["В игре", "ok"],
  stopping: ["Завершение", ""],
};
const GUEST_STATES = {
  working: ["Подключение", ""],
  live: ["В игре", "ok"],
  stopping: ["Завершение", ""],
};

function text(id, value) {
  const el = $(id);
  if (el.textContent !== value) el.textContent = value;
}
const show = (id, on) => { $(id).hidden = !on; };

function line(id, message, error = false) {
  text(id, message);
  show(id, Boolean(message));
  $(id).classList.toggle("error", error);
}

function tone(el, kind) {
  el.classList.remove("ok", "warn", "bad");
  if (kind) el.classList.add(kind);
}

/* ---------- role switch ---------- */

function selectRole(next, focus = false) {
  role = next;
  for (const tab of ["host", "guest"]) {
    const on = tab === next;
    $(`${tab}-tab`).setAttribute("aria-selected", String(on));
    $(`${tab}-tab`).tabIndex = on ? 0 : -1;
    show(`${tab}-panel`, on);
  }
  if (focus) $(`${next}-tab`).focus();
}

/* ---------- guest options ---------- */

function selectProfile(next) {
  profile = PROFILE_NOTES[next] ? next : "balanced";
  document.querySelectorAll("[data-profile]").forEach((b) => {
    b.setAttribute("aria-checked", String(b.dataset.profile === profile));
    b.tabIndex = b.dataset.profile === profile ? 0 : -1;
  });
  text("profile-note", PROFILE_NOTES[profile]);
}

function summarize() {
  const [w, h, fps, mbps] = numbers.map((id) => $(id).value.trim());
  const parts = [];
  if (w && h) parts.push(`${w}×${h}`);
  if (fps) parts.push(`${fps} FPS`);
  if (mbps) parts.push(`${mbps} Мбит/с`);
  if ($("codec").value !== "auto") parts.push($("codec").selectedOptions[0].text);
  text("summary-mode", parts.length ? parts.join(" · ") : "как на экране хоста");
}

function options() {
  const out = { profile, codec: $("codec").value };
  for (const id of numbers) out[id] = $(id).value.trim();
  for (const id of flags) out[id] = $(id).checked;
  return out;
}

function save() {
  try { localStorage.setItem(STORE, JSON.stringify(options())); } catch { /* storage unavailable */ }
}

function restore() {
  let saved = null;
  try { saved = JSON.parse(localStorage.getItem(STORE) || "null"); } catch { /* ignore */ }
  if (saved) {
    for (const id of numbers) if (typeof saved[id] === "string") $(id).value = saved[id];
    for (const id of flags) $(id).checked = Boolean(saved[id]);
    if ([...$("codec").options].some((o) => o.value === saved.codec)) $("codec").value = saved.codec;
  }
  selectProfile(saved?.profile ?? "balanced");
  summarize();
}

/* ---------- host form ---------- */

function selectedNetwork() {
  return state?.networks.find((n) => n.address === network.value);
}

function refreshHostForm() {
  const net = selectedNetwork();
  if (!net) return;
  const loopback = net.kind === "loopback";
  const needsDriver = !loopback && !state.driver;
  $("host-start").disabled = false;
  text("host-start", needsDriver ? "Установить драйвер" : "Создать сессию");
  show("driver-note", needsDriver);
  show("choose-test", needsDriver);
  const hints = {
    tailscale: "Через интернет. Друг должен быть в вашем tailnet или иметь доступ к этому ПК.",
    lan: state.networks.some((n) => n.kind === "tailscale")
      ? "Только для ПК в одной локальной сети. Для интернета выберите Tailscale."
      : "Только для ПК в одной локальной сети. Для игры через интернет установите Tailscale.",
    loopback: "Проверка на одном компьютере: подключитесь во вкладке «Гость».",
  };
  text("network-hint", hints[net.kind]);
}

function updateNetworks(list) {
  const keep = network.value;
  network.replaceChildren(...list.map((n) => new Option(n.label, n.address)));
  if (list.some((n) => n.address === keep)) network.value = keep;
  refreshHostForm();
}

/* ---------- session views ---------- */

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

function renderSession(who, view, states) {
  const active = view.phase !== "idle";
  show(`${who}-form`, !active);
  show(`${who}-session`, active);
  show(`${who}-dot`, active);
  if (!active) {
    line(`${who}-result`, view.message, view.error);
    return;
  }
  const [label, kind] = states[view.phase];
  text(`${who}-state`, label);
  tone($(`${who}-state`), kind);
  text(`${who}-message`, view.message);
  renderFacts(`${who}-facts`, view.facts);
  $(`${who}-stop`).disabled = view.phase === "stopping";
}

function renderStatus(next) {
  const vpn = next.networks.some((n) => n.kind === "tailscale");
  const items = [
    ["st-driver", "driver-state", next.driver, next.driver ? "Установлен" : "Нет", "bad"],
    ["st-vpn", "vpn-state", vpn, vpn ? "Подключён" : "Не найден", "warn"],
    ["st-hags", "hags-state", next.hags === true, next.hags === true ? "Включено" : next.hags === false ? "Выключено" : "Неизвестно", "warn"],
  ];
  for (const [bar, tag, ok, label, fail] of items) {
    tone($(bar), ok ? "ok" : fail);
    $(bar).title = label;
    text(tag, label);
    tone($(tag), ok ? "ok" : fail);
  }
}

function applyState(next) {
  const prev = state;
  state = next;
  if (!prev || JSON.stringify(prev.networks) !== JSON.stringify(next.networks)) updateNetworks(next.networks);
  else if (prev.driver !== next.driver) refreshHostForm();
  renderStatus(next);
  line("setup-result", next.setupMessage, next.setupError);

  renderSession("host", next.host, HOST_STATES);
  show("invitation", next.host.phase === "invite");
  show("approval", next.host.phase === "approval");
  $("invite-output").value = next.host.code;
  text("peer-name", next.host.peer);
  if (next.host.code !== prev?.host.code) text("copy-button", "Копировать");

  renderSession("guest", next.guest, GUEST_STATES);
  const live = next.guest.phase === "live";
  show("guest-shortcut", live);
  text("guest-stop", live ? "Отключиться" : "Отменить");
  guestCode.setAttribute("aria-invalid", String(next.guest.codeError));
  if (prev?.guest.phase === "live" && next.guest.phase === "idle") guestCode.value = "";

  // A request on the host side matters more than whichever tab is open.
  if (next.host.phase === "approval" && prev?.host.phase !== "approval") {
    selectRole("host");
    if (!setup.open) document.querySelector('[data-action="allow"]').focus();
  }
  if (role === "host" && !setup.open && next.host.phase === "invite" && prev?.host.phase !== "invite") $("copy-button").focus();
  if (role === "guest" && next.guest.codeError && !prev?.guest.codeError) guestCode.focus();
}

function toast(message) {
  text("toast", message);
  show("toast", true);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => show("toast", false), 2200);
}

function openSetup() {
  if (!setup.open) setup.showModal();
  send("refresh");
}

/* ---------- events ---------- */

document.querySelectorAll("[data-role]").forEach((tab) => {
  tab.addEventListener("click", () => selectRole(tab.dataset.role));
  tab.addEventListener("keydown", (e) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) return;
    e.preventDefault();
    selectRole(e.key === "Home" ? "host" : e.key === "End" ? "guest" : role === "host" ? "guest" : "host", true);
  });
});

document.querySelectorAll("[data-profile]").forEach((b) => {
  b.addEventListener("click", () => { selectProfile(b.dataset.profile); save(); });
  b.addEventListener("keydown", (e) => {
    const order = ["latency", "balanced", "quality"];
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
  switch (action) {
    case "setup": openSetup(); break;
    case "close_setup": setup.close(); send("refresh"); break;
    case "local_test":
      network.value = "127.0.0.1";
      refreshHostForm();
      $("host-start").focus();
      break;
    case "host_start": {
      const net = selectedNetwork();
      if (net?.kind !== "loopback" && !state.driver) openSetup();
      else send("host_start", { address: network.value });
      break;
    }
    default: send(action);
  }
});

$("guest-form").addEventListener("submit", (e) => {
  e.preventDefault();
  save();
  send("guest_start", { code: guestCode.value, options: options() });
});

network.addEventListener("change", refreshHostForm);
guestCode.addEventListener("input", () => guestCode.removeAttribute("aria-invalid"));
for (const id of [...numbers, "codec"]) $(id).addEventListener("input", () => { summarize(); save(); });
for (const id of flags) $(id).addEventListener("change", save);
$("invite-output").addEventListener("focus", () => $("invite-output").select());

window.duo = {
  applyState,
  toast,
  copied() {
    text("copy-button", "Скопировано");
    toast("Код скопирован");
  },
  pasted(code) {
    guestCode.value = code;
    guestCode.removeAttribute("aria-invalid");
    guestCode.focus();
  },
};

restore();
send("ready");
