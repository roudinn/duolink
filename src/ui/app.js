"use strict";

const $ = (id) => document.getElementById(id);
const send = (action, fields = {}) => window.ipc.postMessage(JSON.stringify({ action, ...fields }));
const settings = $("settings");
const guestCode = $("guest-code");
const NUMBERS = ["width", "height", "fps", "bitrate"];
const FLAGS = ["yuv444", "hdr", "windowed", "stats"];
const STORE = "duolink.video.v1";
const NICK = /^[A-Za-z0-9_.-]{3,16}$/;

let state;
let view = "play";
let confirmRemove = null;
let toastTimer;

const HOST = {
  working: "Создаём сессию",
  invite: "Ждём друга",
  approval: "Запрос на подключение",
  live: "В игре",
  stopping: "Завершаем",
};
const GUEST = {
  working: "Подключаемся",
  live: "В игре",
  stopping: "Отключаемся",
};

const text = (id, value) => { const el = $(id); if (el.textContent !== value) el.textContent = value; };
const show = (id, on) => { $(id).hidden = !on; };
const initial = (name) => [...(name || "?")][0];

function avatar(el, name, online = false) {
  el.textContent = initial(name);
  el.classList.toggle("online", online);
}

function line(id, message, error = false) {
  text(id, message);
  show(id, Boolean(message));
  $(id).classList.toggle("error", error);
}

/* ---------------- video preferences ---------------- */

function options() {
  const out = { codec: $("codec").value };
  for (const id of NUMBERS) out[id] = $(id).value.trim();
  for (const id of FLAGS) out[id] = $(id).checked;
  return out;
}

function save() {
  try { localStorage.setItem(STORE, JSON.stringify(options())); } catch { /* unavailable */ }
}

function restore() {
  let saved = null;
  try { saved = JSON.parse(localStorage.getItem(STORE) || "null"); } catch { /* ignore */ }
  if (!saved) return;
  for (const id of NUMBERS) if (typeof saved[id] === "string") $(id).value = saved[id];
  for (const id of FLAGS) $(id).checked = Boolean(saved[id]);
  if ([...$("codec").options].some((o) => o.value === saved.codec)) $("codec").value = saved.codec;
}

/* ---------------- views ---------------- */

function go(next) {
  view = next;
  confirmRemove = null;
  if (state) render(state);
  if (next === "friends") setTimeout(() => $("add-input").focus(), 50);
}

function renderViews(next) {
  const onboarding = !next.me.chosen;
  show("onboarding", onboarding);
  show("play", !onboarding && view === "play");
  show("friends", !onboarding && view === "friends");
  show("back", !onboarding && view === "friends");
  show("brand", view !== "friends" || onboarding);
  $("friends-tab").classList.toggle("on", view === "friends");
  show("friends-tab", !onboarding);
  const requests = next.friends.filter((f) => f.status === "incoming").length;
  show("friends-badge", requests > 0);
  text("friends-badge", String(requests));
}

/* ---------------- play ---------------- */

function renderPrepare(next) {
  const c = next.components;
  const working = next.setup.phase !== "idle";
  const rows = [
    ["c-engines", c.sunshine && c.moonlight, working ? "Загрузка" : "Нужно"],
    ["c-vigem", c.vigem, "При первой сессии"],
    ["c-firewall", c.firewall, "При первой сессии"],
  ];
  for (const [id, ok, pending] of rows) {
    const busy = !ok && working && (id === "c-engines" || next.fullSetup);
    $(id).classList.toggle("done", ok);
    $(id).classList.toggle("busy", busy);
    $(id).querySelector("em").textContent = ok ? "Готово" : busy ? "Установка" : pending;
  }
  show("prepare", working || next.setup.error);
  text("setup-line", working || next.setup.error ? next.setup.message : "");
  show("install", next.setup.error && !working);
}

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
    return [dt, dd];
  }));
}

function renderSession(who, pane, table) {
  const active = pane.phase !== "idle";
  show(`${who}-live`, active);
  if (!active) {
    line(`${who}-result`, pane.message, pane.error);
    return;
  }
  text(`${who}-title`, table[pane.phase]);
  text(`${who}-message`, pane.message);
  show(`${who}-message`, Boolean(pane.message));
  renderFacts(`${who}-facts`, pane.facts);
  $(`${who}-stop`).disabled = pane.phase === "stopping";
}

function renderPlay(next) {
  renderPrepare(next);
  const hostActive = next.host.phase !== "idle";
  const guestActive = next.guest.phase !== "idle";

  renderSession("host", next.host, HOST);
  const forFriend = Boolean(next.hostFor);
  show("invitation", next.host.phase === "invite" && !forFriend);
  show("approval", next.host.phase === "approval");
  text("code-box", next.host.code);
  if (next.host.phase === "approval") text("host-title", `${next.host.peer.split(" · ")[0]} хочет подключиться`);
  if (next.host.phase === "invite" && forFriend) text("host-title", `Ждём ${next.hostFor.name}`);

  renderSession("guest", next.guest, GUEST);
  show("guest-keys", next.guest.phase === "live");
  text("guest-stop", next.guest.phase === "live" ? "Отключиться" : "Отменить");
  guestCode.setAttribute("aria-invalid", String(next.guest.codeError));

  // While waiting for a code-based guest, "По коду" stays available for a same-PC test.
  show("home", !guestActive && (!hostActive || (next.host.phase === "invite" && !forFriend)));
  show("host-card", !hostActive);

  const invite = next.invitation;
  show("friend-invite", Boolean(invite) && !guestActive);
  if (invite) {
    text("invite-name", invite.name);
    avatar($("invite-avatar"), invite.name, true);
  }
}

/* ---------------- friends ---------------- */

function button(label, action, id, primary = false, disabled = false) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = `btn small${primary ? " primary" : ""}`;
  b.textContent = label;
  b.dataset.action = action;
  if (id) b.dataset.id = id;
  b.disabled = disabled;
  return b;
}

function removeButton(friend) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "round x";
  b.dataset.action = "ask_remove";
  b.dataset.id = friend.id;
  b.setAttribute("aria-label", "Удалить");
  b.innerHTML = '<svg><use href="#i-close"/></svg>';
  return b;
}

function friendRow(friend, hosting) {
  const row = document.createElement("div");
  row.className = "friend";
  const face = document.createElement("span");
  face.className = "avatar";
  avatar(face, friend.name, friend.online);
  const who = document.createElement("div");
  who.className = "who";
  const name = document.createElement("b");
  name.textContent = friend.name;
  const tag = document.createElement("span");
  tag.className = "tag";
  tag.textContent = `#${friend.tag}`;
  name.append(tag);
  const note = document.createElement("small");
  who.append(name, note);
  row.append(face, who);

  if (confirmRemove === friend.id) {
    row.classList.add("confirm");
    note.textContent = friend.status === "incoming" ? "Отклонить запрос?" : "Удалить из друзей?";
    const yes = button(friend.status === "incoming" ? "Отклонить" : "Удалить", "remove_friend", friend.id);
    yes.classList.add("danger");
    row.append(button("Отмена", "cancel_remove"), yes);
    return row;
  }
  if (friend.status === "incoming") {
    note.textContent = "хочет дружить";
    row.append(button("Принять", "accept_friend", friend.id, true), removeButton(friend));
  } else if (friend.status === "outgoing") {
    note.textContent = friend.sending ? "отправляем…" : "запрос отправлен";
    row.append(removeButton(friend));
  } else {
    note.textContent = friend.online ? "в сети" : "не в сети";
    row.append(button("Позвать", "invite_friend", friend.id, friend.online, hosting), removeButton(friend));
  }
  return row;
}

function renderFriends(next) {
  text("me-name", next.me.name);
  text("me-tag", `#${next.me.tag}`);
  avatar($("me-avatar"), next.me.name);
  $("add-button").disabled = next.lookingUp;

  const hosting = next.host.phase !== "idle";
  const byName = (a, b) => a.name.localeCompare(b.name);
  const sections = [
    ["Запросы", next.friends.filter((f) => f.status === "incoming")],
    ["В сети", next.friends.filter((f) => f.status === "friend" && f.online)],
    ["Не в сети", next.friends.filter((f) => f.status === "friend" && !f.online)],
    ["Исходящие", next.friends.filter((f) => f.status === "outgoing")],
  ].filter(([, list]) => list.length);

  const key = JSON.stringify([sections, hosting, confirmRemove]);
  const root = $("friend-sections");
  if (root.dataset.key !== key) {
    root.dataset.key = key;
    root.replaceChildren(...sections.flatMap(([title, list]) => {
      const label = document.createElement("p");
      label.className = "section";
      label.textContent = title;
      const group = document.createElement("div");
      group.className = "group";
      group.append(...list.sort(byName).map((f) => friendRow(f, hosting)));
      return [label, group];
    }));
  }
  show("friends-empty", next.friends.length === 0);
}

/* ---------------- settings ---------------- */

function renderSettings(next) {
  if (document.activeElement !== $("nick-edit")) $("nick-edit").value = next.me.name;
  const hags = next.components.hags;
  text("hags-note", hags === true ? "включено" : hags === false ? "выключено — включите на хосте" : "");
  text("version-note", `версия ${next.version}`);
  const u = next.update;
  const notes = {
    idle: "автоматически",
    checking: "проверяем…",
    current: "последняя версия",
    downloading: `скачиваем ${u.version}…`,
    ready: `${u.version} — после перезапуска`,
    failed: "нет связи",
  };
  text("update-note", notes[u.state] ?? "");
  $("update-check").disabled = ["checking", "downloading", "ready"].includes(u.state);
  show("update-banner", u.state === "ready");
  if (u.state === "ready") text("update-title", `Доступна версия ${u.version}`);
}

/* ---------------- state ---------------- */

function render(next) {
  renderViews(next);
  renderPlay(next);
  renderFriends(next);
  renderSettings(next);
  const preview = $("nick-input").value.trim() || "nickname";
  text("nick-preview", `${preview}#${next.me.tag}`);
}

function applyState(next) {
  const prev = state;
  state = next;
  render(next);
  if (prev?.guest.phase === "live" && next.guest.phase === "idle") guestCode.value = "";
  if (next.host.phase === "approval" && prev?.host.phase !== "approval") {
    go("play");
    document.querySelector('[data-action="allow"]').focus();
  }
  if (next.invitation && !prev?.invitation) go("play");
  if (!next.me.chosen && (!prev || prev.me.chosen)) $("nick-input").focus();
  if (next.guest.codeError && !prev?.guest.codeError) guestCode.focus();
}

function toast(message) {
  text("toast", message);
  $("toast").hidden = true;
  void $("toast").offsetWidth;
  $("toast").hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => show("toast", false), 2600);
}

/* ---------------- events ---------------- */

document.addEventListener("click", (e) => {
  const b = e.target.closest("button[data-action]");
  if (!b || b.disabled) return;
  const { action, id } = b.dataset;
  switch (action) {
    case "settings": settings.showModal(); break;
    case "close_settings": settings.close(); break;
    case "go_friends": go(view === "friends" ? "play" : "friends"); break;
    case "go_play": go("play"); break;
    case "ask_remove": confirmRemove = id; render(state); break;
    case "cancel_remove": confirmRemove = null; render(state); break;
    case "remove_friend": confirmRemove = null; send("remove_friend", { id }); break;
    case "accept_invite": send("accept_invite", { options: options() }); break;
    case "invite_friend": send("invite_friend", { id }); go("play"); break;
    default: send(action, id ? { id } : {});
  }
});

settings.addEventListener("click", (e) => { if (e.target === settings) settings.close(); });
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && view === "friends" && !settings.open) go("play");
});

$("guest-form").addEventListener("submit", (e) => {
  e.preventDefault();
  save();
  send("guest_start", { code: guestCode.value, options: options() });
});
guestCode.addEventListener("input", () => guestCode.removeAttribute("aria-invalid"));

$("add-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const query = $("add-input").value.trim();
  if (query) send("add_friend", { query });
});

$("nick-input").addEventListener("input", () => state && render(state));
$("nick-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const name = $("nick-input").value.trim();
  if (!NICK.test(name)) return toast("3–16 символов: латиница, цифры, _ . -");
  send("set_name", { name });
});
$("nick-settings").addEventListener("submit", (e) => {
  e.preventDefault();
  const name = $("nick-edit").value.trim();
  if (!NICK.test(name)) return toast("3–16 символов: латиница, цифры, _ . -");
  send("set_name", { name });
  $("nick-edit").blur();
  toast("Сохранено");
});

for (const id of [...NUMBERS, "codec"]) $(id).addEventListener("input", save);
for (const id of FLAGS) $(id).addEventListener("change", save);

window.duo = {
  applyState,
  toast,
  friendAdded() { $("add-input").value = ""; },
  pasted(code) {
    guestCode.value = code;
    guestCode.removeAttribute("aria-invalid");
    guestCode.focus();
  },
};

restore();
send("ready");
