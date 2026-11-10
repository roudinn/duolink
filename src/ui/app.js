"use strict";

const get = (id) => document.getElementById(id);
const send = (action, fields = {}) => window.ipc.postMessage(JSON.stringify({ action, ...fields }));
const setup = get("setup-dialog");
const network = get("network");
const guestCode = get("guest-code");
const quality = ["width", "height", "fps", "bitrate"].map(get);
let currentRole = "host";
let state;
let toastTimer;

const hostHeadings = {
  working: ["Подготовка", "Готовим сессию"],
  invite: ["Ожидание", "Пригласите друга"],
  approval: ["Запрос на вход", "Разрешить подключение?"],
  live: ["В эфире", "Гость подключён"],
  stopping: ["Завершение", "Закрываем сессию"],
};
const guestHeadings = {
  working: ["Подключение", "Подключаемся к хосту"],
  live: ["Подключено", "Можно играть"],
  stopping: ["Завершение", "Отключаемся"],
};

function text(id, value) {
  const element = get(id);
  if (element.textContent !== value) element.textContent = value;
}

function visible(id, value) {
  get(id).hidden = !value;
}

function selectRole(role, focus = false) {
  currentRole = role;
  for (const tab of ["host", "guest"]) {
    const selected = role === tab;
    const button = get(`${tab}-tab`);
    button.setAttribute("aria-selected", String(selected));
    button.tabIndex = selected ? 0 : -1;
    visible(`${tab}-panel`, selected);
  }
  if (focus) get(`${role}-tab`).focus();
}

function result(id, message, error = false) {
  text(id, message);
  visible(id, Boolean(message));
  get(id).classList.toggle("error", error);
}

function setupHostForm() {
  const selected = state?.networks.find((entry) => entry.address === network.value);
  if (!selected) return;
  const loopback = selected.kind === "loopback";
  const needsDriver = !loopback && !state.driver;
  get("host-start").disabled = false;
  get("host-start").querySelector("span").textContent = needsDriver ? "Настроить хост" : "Создать сессию";
  visible("driver-note", needsDriver);
  visible("choose-test", needsDriver);
  const hints = {
    tailscale: "Подходит для игры из разных сетей.",
    lan: state.networks.some((entry) => entry.kind === "tailscale")
      ? "Для компьютеров в одной сети. Для игры через интернет выберите Tailscale."
      : "Для компьютеров в одной сети. Чтобы играть через интернет, настройте Tailscale.",
    loopback: "Проверка на одном ПК. Гость может подключиться во вкладке «Гость».",
  };
  text("network-hint", hints[selected.kind]);
}

function updateNetworks(entries) {
  const selection = network.value;
  network.replaceChildren(...entries.map((entry) => new Option(entry.label, entry.address)));
  if (entries.some((entry) => entry.address === selection)) network.value = selection;
  setupHostForm();
}

function renderSession(role, view, headings) {
  const active = view.phase !== "idle";
  visible(`${role}-form`, !active);
  visible(`${role}-session`, active);
  visible(`${role}-dot`, active);
  if (active) {
    const [badge, heading] = headings[view.phase];
    text(`${role}-title`, heading);
    text(`${role}-badge-text`, badge);
    text(`${role}-message`, view.message);
    get(`${role}-badge`).classList.toggle("live", view.phase === "live");
    get(`${role}-badge`).classList.toggle("request", view.phase === "approval");
    get(`${role}-stop`).disabled = view.phase === "stopping";
  } else {
    result(`${role}-result`, view.message, view.error);
  }
}

function applyState(next) {
  const prev = state;
  state = next;
  if (!prev || JSON.stringify(prev.networks) !== JSON.stringify(next.networks)) {
    updateNetworks(next.networks);
  } else if (prev.driver !== next.driver) {
    setupHostForm();
  }
  visible("vpn-ready", next.networks.some((entry) => entry.kind === "tailscale"));
  visible("driver-ready", next.driver);
  result("setup-result", next.setupMessage, next.setupError);

  renderSession("host", next.host, hostHeadings);
  visible("invitation", next.host.phase === "invite");
  visible("approval", next.host.phase === "approval");
  visible("approval-actions", next.host.phase === "approval");
  get("invite-output").value = next.host.code;
  text("peer-name", next.host.peer);
  if (next.host.code !== prev?.host.code) text("copy-text", "Копировать");

  renderSession("guest", next.guest, guestHeadings);
  const live = next.guest.phase === "live";
  visible("guest-shortcut", live);
  text("guest-stop", live ? "Отключиться" : "Отменить");
  guestCode.setAttribute("aria-invalid", String(next.guest.codeError));

  if (prev?.guest.phase === "live" && next.guest.phase === "idle") guestCode.value = "";
  if (currentRole === "host" && !setup.open) {
    if (next.host.phase === "invite" && prev?.host.phase !== "invite") get("copy-button").focus();
    if (next.host.phase === "approval" && prev?.host.phase !== "approval") {
      document.querySelector('[data-action="allow"]').focus();
    }
  }
  if (currentRole === "guest" && next.guest.codeError && !prev?.guest.codeError) guestCode.focus();
}

function toast(message) {
  text("toast", message);
  visible("toast", true);
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => visible("toast", false), 2300);
}

function showSetup() {
  if (!setup.open) setup.showModal();
  send("refresh");
}

document.querySelectorAll("[data-role]").forEach((tab) => {
  tab.addEventListener("click", () => selectRole(tab.dataset.role));
  tab.addEventListener("keydown", (event) => {
    const keys = ["ArrowLeft", "ArrowRight", "Home", "End"];
    if (!keys.includes(event.key)) return;
    event.preventDefault();
    const target = event.key === "Home" ? "host" : event.key === "End" ? "guest" : currentRole === "host" ? "guest" : "host";
    selectRole(target, true);
  });
});

document.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-action]");
  if (!button) return;
  const { action } = button.dataset;
  if (action === "setup") {
    showSetup();
  } else if (action === "close_setup") {
    setup.close();
    send("refresh");
  } else if (action === "local_test") {
    network.value = "127.0.0.1";
    setupHostForm();
    get("host-start").focus();
  } else if (action === "host_start") {
    const selected = state.networks.find((entry) => entry.address === network.value);
    if (selected?.kind !== "loopback" && !state.driver) showSetup();
    else send("host_start", { address: network.value });
  } else {
    send(action);
  }
});

get("guest-form").addEventListener("submit", (event) => {
  event.preventDefault();
  send("guest_start", {
    code: guestCode.value,
    width: get("width").value,
    height: get("height").value,
    fps: get("fps").value,
    bitrate: get("bitrate").value,
    stats: get("stats").checked,
  });
});
network.addEventListener("change", setupHostForm);
guestCode.addEventListener("input", () => guestCode.removeAttribute("aria-invalid"));
for (const field of quality) {
  field.addEventListener("input", () => text("summary-mode", quality.some((element) => element.value.trim()) ? "Вручную" : "Авто"));
}
get("invite-output").addEventListener("click", () => get("invite-output").select());

window.duo = {
  applyState,
  toast,
  copied() {
    text("copy-text", "Скопировано");
    toast("Код скопирован");
  },
  pasted(code) {
    guestCode.value = code;
    guestCode.removeAttribute("aria-invalid");
    guestCode.focus();
  },
};
send("ready");
