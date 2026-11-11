"use strict";

const $ = (id) => document.getElementById(id);
const send = (action, fields = {}) => window.ipc.postMessage(JSON.stringify({ action, ...fields }));
const settings = $("settings");
const guestCode = $("guest-code");
const NUMBERS = ["width", "height", "fps", "bitrate"];
const FLAGS = ["yuv444", "hdr", "windowed", "stats"];
const STORE = "duolink.guest.v3";

let state;
let toastTimer;

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

function options() {
  const out = { codec: $("codec").value };
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
}

/* ---------- home ---------- */

function renderHostButton() {
  const waiting = state.hostAfterSetup;
  $("host-start").disabled = waiting;
  text("host-start", waiting ? "Устанавливаем драйвер геймпада…" : "Создать сессию");
}

function renderPrepare(next) {
  const c = next.components;
  const working = next.setup.phase !== "idle";
  const rows = [
    ["c-engines", c.sunshine && c.moonlight, working ? "Загрузка" : "Нужно скачать"],
    ["c-vigem", c.vigem, "При создании сессии"],
    ["c-firewall", c.firewall, "При создании сессии"],
  ];
  let done = 0;
  for (const [id, ok, pending] of rows) {
    const li = $(id);
    const busy = !ok && working && (id === "c-engines" || next.fullSetup);
    tone(li, ok ? "done" : busy ? "busy" : "");
    li.querySelector("em").textContent = ok ? "Готово" : busy ? "Установка" : pending;
    if (ok) done += 1;
  }
  // Setup is invisible unless something is happening or went wrong.
  show("prepare", working || next.setup.error);
  text("prepare-count", `${done} из ${rows.length}`);
  show("install", next.setup.error && !working);
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
    if (name === "Соединение") tone(dd, "ok");
    if (name === "Доступ") tone(dd, value.startsWith("Строгий") || value.startsWith("Только") ? "warn" : "ok");
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
  renderHostButton();
  renderPrepare(next);

  const hostActive = next.host.phase !== "idle";
  const guestActive = next.guest.phase !== "idle";
  show("live", hostActive || guestActive);
  // While a host waits for a friend, «Подключиться» stays reachable to test on this same PC.
  show("home", !guestActive && (!hostActive || (next.host.phase === "invite" && !next.hostFor)));
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
  renderFriends(next);
  renderUpdate(next.update);

  if (next.host.phase === "approval" && prev?.host.phase !== "approval" && !settings.open) {
    document.querySelector('[data-action="allow"]').focus();
  }
  if (next.host.phase === "invite" && prev?.host.phase !== "invite" && !settings.open) $("copy-button").focus();
  if (next.guest.codeError && !prev?.guest.codeError) guestCode.focus();
}

function renderUpdate(update) {
  const ready = update.state === "ready";
  show("update-banner", ready);
  if (ready) text("update-title", `DuoLink ${update.version} готов`);
  const notes = {
    idle: "Проверяются автоматически",
    checking: "Проверяем…",
    current: "У вас последняя версия",
    downloading: `Скачиваем ${update.version}…`,
    ready: `Версия ${update.version} установится при перезапуске`,
    failed: `Не удалось проверить: ${update.version ?? ""}`,
  };
  text("update-note", notes[update.state] ?? "");
  $("update-check").disabled = update.state === "checking" || update.state === "downloading" || ready;
}

const X_ICON = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 7 10 10M17 7 7 17"/></svg>';

function friendRow(friend, hosting) {
  const li = document.createElement("li");
  const avatar = document.createElement("span");
  avatar.className = "avatar";
  avatar.textContent = [...friend.name][0] ?? "?";
  const info = document.createElement("div");
  const name = document.createElement("b");
  name.textContent = friend.name;
  info.append(name);
  const button = (label, action, primary) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = `btn small${primary ? " primary" : ""}`;
    b.textContent = label;
    b.dataset.action = action;
    b.dataset.id = friend.id;
    return b;
  };
  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "icon-btn remove";
  remove.dataset.action = "remove_friend";
  remove.dataset.id = friend.id;
  remove.dataset.name = friend.name;
  remove.innerHTML = X_ICON;
  li.append(avatar, info);
  if (friend.status === "friend") {
    remove.title = "Удалить из друзей";
    const invite = button("Пригласить", "invite_friend", true);
    invite.disabled = hosting;
    li.append(invite, remove);
  } else if (friend.status === "incoming") {
    const note = document.createElement("small");
    note.textContent = "хочет добавить вас в друзья";
    info.append(note);
    remove.title = "Отклонить";
    li.append(button("Принять", "accept_friend", true), remove);
  } else {
    const note = document.createElement("small");
    note.textContent = "запрос отправлен, ждём ответа";
    info.append(note);
    remove.title = "Отменить запрос";
    li.append(remove);
  }
  return li;
}

function renderFriends(next) {
  show("nick-card", !next.nicknameChosen);
  text("me-nick", `Вы: ${next.nickname}`);
  if (document.activeElement !== $("nick-edit")) $("nick-edit").value = next.nickname;

  const hosting = next.host.phase !== "idle";
  const order = { incoming: 0, friend: 1, outgoing: 2 };
  const list = [...next.friends].sort((a, b) => order[a.status] - order[b.status] || a.name.localeCompare(b.name));
  const key = JSON.stringify([list, hosting]);
  const ul = $("friend-list");
  if (ul.dataset.key !== key) {
    ul.dataset.key = key;
    ul.replaceChildren(...list.map((f) => friendRow(f, hosting)));
  }
  show("friend-list", list.length > 0);
  show("friends-empty", list.length === 0);
  if (list.length === 0 && !prevFriendsShown) $("add-friend").open = true;
  prevFriendsShown = true;

  const invite = next.invitation;
  show("friend-invite", Boolean(invite) && next.guest.phase === "idle");
  if (invite) {
    text("friend-invite-title", `${invite.name} зовёт играть`);
    text("friend-invite-avatar", [...invite.name][0] ?? "?");
  }

  // A session created for a friend: no code to pass around.
  const target = next.hostFor;
  if (target && next.host.phase === "invite") {
    text("host-title", `Ждём ${target.name}`);
    show("invitation", false);
  }
}

let prevFriendsShown = false;

function toast(message) {
  text("toast", message);
  show("toast", true);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => show("toast", false), 2600);
}

/* ---------- events ---------- */

document.addEventListener("click", (e) => {
  const button = e.target.closest("button[data-action]");
  if (!button) return;
  const { action, id } = button.dataset;
  if (action === "settings") settings.showModal();
  else if (action === "close_settings") settings.close();
  else if (action === "edit_nick") { settings.showModal(); $("nick-edit").focus(); $("nick-edit").select(); }
  else if (action === "add_friend") send("add_friend", { code: $("friend-code").value });
  else if (action === "accept_invite") send("accept_invite", { options: options() });
  else if (action === "remove_friend") {
    if (window.confirm(`Удалить ${button.dataset.name} из друзей?`)) send("remove_friend", { id });
  }
  else if (id) send(action, { id });
  else send(action);
});

settings.addEventListener("click", (e) => { if (e.target === settings) settings.close(); });
$("guest-form").addEventListener("submit", (e) => {
  e.preventDefault();
  save();
  send("guest_start", { code: guestCode.value, options: options() });
});
guestCode.addEventListener("input", () => guestCode.removeAttribute("aria-invalid"));
for (const id of [...NUMBERS, "codec"]) $(id).addEventListener("input", save);
for (const id of FLAGS) $(id).addEventListener("change", save);
window.addEventListener("focus", () => { if (state && !settings.open) send("refresh"); });

for (const form of ["nick-form", "nick-settings"]) {
  $(form).addEventListener("submit", (e) => {
    e.preventDefault();
    const input = $(form === "nick-form" ? "nick-input" : "nick-edit");
    send("set_name", { name: input.value });
    input.blur();
  });
}

window.duo = {
  applyState,
  friendAdded() {
    $("friend-code").value = "";
  },
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
