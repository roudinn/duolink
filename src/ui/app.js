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
const main = document.querySelector("main");
// Wide windows show the friends panel beside the main area at all times.
const wideQuery = window.matchMedia("(min-width: 721px)");
const wide = () => wideQuery.matches;

const HOST = {
  working: "Создаём сессию",
  invite: "Ждём друга",
  approval: "Подключение",
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

function avatar(el, name) {
  el.textContent = initial(name);
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
  main.scrollTop = 0;
  if (settings.open) return;
  const target = next === "friends" && !wide() ? "friends-title" :
    !$("host-live").hidden ? "host-title" : !$("guest-live").hidden ? "guest-title" :
      !$("prepare").hidden ? "setup-title" : "host-start";
  $(target).focus();
}

function renderViews(next) {
  show("play", wide() || view === "play");
  show("friends", wide() || view === "friends");
  show("nick-form", !next.me.chosen);
  show("friends-content", next.me.chosen);
  $("nav-icon").setAttribute("href", view === "friends" ? "#i-back" : "#i-people");
  const requests = next.friends.filter((f) => f.status === "incoming").length;
  text("nav-label", view === "friends" ? tr("К игре") : tr("Друзья") + (requests ? ` (${requests})` : ""));
}

/* ---------------- play ---------------- */

function sessionBusy(next) {
  return [next.host, next.guest, next.setup].some((pane) => pane.phase !== "idle");
}

function renderFacts(id, facts) {
  const dl = $(id);
  const key = JSON.stringify([language.value, facts]);
  if (dl.dataset.key === key) return;
  dl.dataset.key = key;
  dl.replaceChildren(...facts.flatMap(([name, value]) => {
    const dt = document.createElement("dt");
    dt.textContent = tr(name);
    const dd = document.createElement("dd");
    dd.textContent = factValue(name, value);
    return [dt, dd];
  }));
}

function renderSession(who, pane, table) {
  const active = pane.phase !== "idle";
  show(`${who}-live`, active);
  if (!active) {
    line(`${who}-result`, notice(pane.message), pane.error);
    $(`${who}-details`).open = false;
    return;
  }
  const title = table[pane.phase];
  text(`${who}-title`, tr(title));
  const message = pane.message.replace(/[…\s]+$/, "") === title ? "" : pane.message;
  line(`${who}-message`, notice(message));
  show(`${who}-details`, pane.facts.length > 0);
  renderFacts(`${who}-facts`, pane.facts);
  $(`${who}-stop`).disabled = pane.phase === "stopping";
}

function renderPlay(next) {
  const preparing = next.setup.phase !== "idle";
  show("prepare", preparing);
  text("setup-line", preparing ? notice(next.setup.message) : "");
  $("setup-stop").disabled = next.setup.phase === "stopping";

  renderSession("host", next.host, HOST);
  const forFriend = Boolean(next.hostFor);
  show("invitation", next.host.phase === "invite" && !forFriend);
  show("approval", next.host.phase === "approval");
  text("code-box", next.host.code);
  if (next.host.phase === "approval") {
    const peer = next.host.peer === "Гость" ? tr("Гость") : next.host.peer;
    line("host-message", pick(`${peer} хочет подключиться.`, `${peer} wants to connect.`));
  }
  if (next.host.phase === "invite" && forFriend) {
    text("host-title", pick(`Ждём ${next.hostFor.name}`, `Waiting for ${next.hostFor.name}`));
  }

  renderSession("guest", next.guest, GUEST);
  show("guest-keys", next.guest.phase === "live");
  text("guest-stop", tr(next.guest.phase === "live" ? "Отключиться" : "Отменить"));
  if (next.guest.codeError) guestCode.setAttribute("aria-invalid", "true");
  else guestCode.removeAttribute("aria-invalid");
  show("fix-video", next.guest.videoError);
  show("home", !sessionBusy(next));
}

/* ---------------- friends ---------------- */

function button(label, action, id, primary = false, disabled = false) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = `btn small${primary ? " primary" : ""}`;
  b.textContent = tr(label);
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
  const label = friend.status === "incoming" ? "Отклонить запрос" : friend.status === "outgoing" ? "Отозвать запрос" : "Удалить из друзей";
  b.setAttribute("aria-label", `${tr(label)} ${friend.name}#${friend.tag}`);
  b.title = tr(label);
  b.disabled = friend.sending;
  b.innerHTML = '<svg aria-hidden="true"><use href="#i-close"/></svg>';
  return b;
}

function friendRow(friend, busy) {
  const row = document.createElement("div");
  row.className = "friend";
  const face = document.createElement("span");
  face.className = "avatar";
  face.setAttribute("aria-hidden", "true");
  avatar(face, friend.name);
  const who = document.createElement("div");
  who.className = "who";
  const name = document.createElement("b");
  name.textContent = friend.name;
  const tag = document.createElement("span");
  tag.className = "tag";
  tag.textContent = `#${friend.tag}`;
  name.append(tag);
  who.append(name);
  row.append(face, who);

  if (confirmRemove === friend.id) {
    row.classList.add("confirm");
    const label = friend.status === "incoming" ? "Отклонить" : friend.status === "outgoing" ? "Отозвать" : "Удалить";
    row.append(button("Отмена", "cancel_remove", friend.id), button(label, "remove_friend", friend.id));
    return row;
  }
  if (friend.status === "incoming") {
    row.append(button(friend.sending ? "Принимаем…" : "Принять", "accept_friend", friend.id, true, friend.sending), removeButton(friend));
  } else if (friend.status === "outgoing") {
    if (friend.sending) {
      const note = document.createElement("small");
      note.textContent = tr("Отправляем…");
      who.append(note);
    }
    row.append(removeButton(friend));
  } else {
    const invite = button("Позвать", "invite_friend", friend.id, false, busy);
    if (busy) invite.title = tr("Сначала завершите сессию");
    row.append(invite, removeButton(friend));
  }
  return row;
}

function replaceRows(root, rows) {
  const active = document.activeElement;
  const { action, id } = active?.dataset ?? {};
  const restoreFocus = root.contains(active);
  root.replaceChildren(...rows);
  if (restoreFocus) focusAction(root, action, id);
}

function focusAction(root, action, id) {
  const match = [...root.querySelectorAll("button[data-action]:enabled")]
    .find((button) => button.dataset.action === action && button.dataset.id === id);
  match?.focus();
}

function renderInvites(next) {
  const busy = sessionBusy(next);
  const invites = $("invites");
  const key = JSON.stringify([language.value, next.invitations, busy]);
  show("invites", next.invitations.length > 0);
  if (invites.dataset.key === key) return;
  invites.dataset.key = key;
  replaceRows(invites, next.invitations.map(({ id, name }) => {
    const row = document.createElement("div");
    row.className = "notice";
    const who = document.createElement("p");
    const nick = document.createElement("b");
    nick.textContent = name;
    who.append(nick, " " + tr("зовёт играть"));
    const join = button("Играть", "accept_invite", id, true, busy);
    if (busy) join.title = tr("Сначала завершите сессию");
    row.append(who, button("Отклонить", "decline_invite", id), join);
    return row;
  }));
}

function renderFriends(next) {
  text("me-name", next.me.name);
  text("me-tag", `#${next.me.tag}`);
  avatar($("me-avatar"), next.me.name);
  $("me-chip").setAttribute("aria-label", `${tr("Скопировать свой ник")} ${next.me.name}#${next.me.tag}`);
  $("add-button").disabled = next.lookingUp;
  text("add-button", tr(next.lookingUp ? "Ищем…" : "Добавить"));

  const busy = sessionBusy(next);
  const byName = (a, b) => a.name.localeCompare(b.name);
  const sections = [
    ["Запросы в друзья", next.friends.filter((f) => f.status === "incoming")],
    ["Друзья", next.friends.filter((f) => f.status === "friend")],
    ["Исходящие запросы", next.friends.filter((f) => f.status === "outgoing")],
  ].filter(([, list]) => list.length);

  const key = JSON.stringify([language.value, sections, busy, confirmRemove]);
  const root = $("friend-sections");
  if (root.dataset.key !== key) {
    root.dataset.key = key;
    replaceRows(root, sections.flatMap(([title, list]) => {
      const label = document.createElement("h2");
      label.className = "section";
      label.textContent = tr(title);
      const group = document.createElement("div");
      group.className = "group";
      group.append(...list.sort(byName).map((f) => friendRow(f, busy)));
      // The panel is already titled "Друзья"; a second heading would repeat it.
      return title === "Друзья" ? [group] : [label, group];
    }));
  }
  show("friends-empty", next.friends.length === 0);
}

/* ---------------- settings ---------------- */

function renderSettings(next) {
  show("copy-friend-code", next.me.chosen);
  const u = next.update;
  const busy = sessionBusy(next);
  const ready = u.state === "ready";
  const notes = {
    checking: tr("Проверяем…"),
    current: pick(`${next.version} · последняя версия`, `${next.version} · up to date`),
    downloading: pick(`Скачиваем ${u.version}…`, `Downloading ${u.version}…`),
    ready: busy ? tr("Завершите сессию для обновления") : pick(`${u.version} после перезапуска`, `Restart to use ${u.version}`),
    failed: tr("Не удалось обновить. Проверьте ещё раз."),
  };
  text("update-note", notes[u.state] ?? pick(`Версия ${next.version}`, `Version ${next.version}`));
  text("update-check", tr(ready ? "Перезапустить" : "Проверить"));
  $("update-check").dataset.action = ready ? "restart" : "check_update";
  $("update-check").disabled = ["checking", "downloading"].includes(u.state) || (ready && busy);
  show("update-banner", ready && !busy);
  if (ready) text("update-title", pick(`Обновление ${u.version}`, `Update ${u.version}`));
  line("video-feedback", next.guest.videoError ? notice(next.guest.message) : "", next.guest.videoError);
}

/* ---------------- state ---------------- */

function render(next) {
  renderViews(next);
  renderPlay(next);
  renderInvites(next);
  renderFriends(next);
  renderSettings(next);
}

function applyState(next) {
  const prev = state;
  state = next;
  if (prev?.me.name !== next.me.name || prev?.me.chosen !== next.me.chosen) {
    $("nick-edit").value = next.me.chosen ? next.me.name : "";
  }
  render(next);
  if (prev?.guest.phase === "live" && next.guest.phase === "idle") guestCode.value = "";
  if (next.host.phase === "approval" && prev?.host.phase !== "approval") {
    go("play");
  }
  if (next.me.chosen && !prev?.me.chosen && view === "friends" && !settings.open) $("add-input").focus();
  if (next.guest.codeError && !prev?.guest.codeError && !settings.open && view === "play") guestCode.focus();
}

function toast(message) {
  message = notice(message);
  if (settings.open) {
    line("settings-feedback", message);
    return;
  }
  text("toast", message);
  $("toast").hidden = true;
  void $("toast").offsetWidth;
  $("toast").hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => show("toast", false), 4000);
}

function cancelRemove(id) {
  confirmRemove = null;
  render(state);
  focusAction($("friend-sections"), "ask_remove", id);
}

/* ---------------- events ---------------- */

document.addEventListener("click", (e) => {
  const b = e.target.closest("button[data-action]");
  if (!b || b.disabled) return;
  const { action, id } = b.dataset;
  switch (action) {
    case "settings": settings.showModal(); break;
    case "video_settings":
      settings.showModal();
      $("video-title").focus();
      $("video-title").scrollIntoView({ block: "start" });
      break;
    case "close_settings": settings.close(); break;
    case "toggle_view": go(view === "friends" ? "play" : "friends"); break;
    case "ask_remove":
      confirmRemove = id;
      render(state);
      focusAction($("friend-sections"), "cancel_remove", id);
      break;
    case "cancel_remove": cancelRemove(id); break;
    case "remove_friend":
      confirmRemove = null;
      send("remove_friend", { id });
      $("add-input").focus();
      break;
    case "accept_invite":
      send("accept_invite", { id, options: options() });
      if (!wide()) go("play");
      break;
    case "invite_friend":
      send("invite_friend", { id });
      if (!wide()) go("play");
      break;
    default: send(action, id ? { id } : {});
  }
});

settings.addEventListener("click", (e) => { if (e.target === settings) settings.close(); });
settings.addEventListener("close", () => line("settings-feedback", ""));
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || settings.open || (view !== "friends" && !confirmRemove)) return;
  if (confirmRemove) {
    cancelRemove(confirmRemove);
  } else if (!wide()) {
    go("play");
  }
});
wideQuery.addEventListener("change", () => {
  view = "play";
  if (state) render(state);
});

$("guest-form").addEventListener("submit", (e) => {
  e.preventDefault();
  save();
  send("guest_start", { code: guestCode.value, options: options() });
});
guestCode.addEventListener("input", () => {
  guestCode.removeAttribute("aria-invalid");
  if (state?.guest.codeError) send("clear_code_error");
});

$("add-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const query = $("add-input").value.trim();
  if (query) send("add_friend", { query });
});

$("nick-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const name = $("nick-input").value.trim();
  if (!NICK.test(name)) return toast($("nick-hint").textContent);
  send("set_name", { name });
});
$("nick-settings").addEventListener("submit", (e) => {
  e.preventDefault();
  const name = $("nick-edit").value.trim();
  if (!NICK.test(name)) return toast($("nick-hint").textContent);
  send("set_name", { name });
});

for (const id of [...NUMBERS, "codec"]) $(id).addEventListener("input", save);
for (const id of FLAGS) $(id).addEventListener("change", save);
language.addEventListener("change", () => {
  try { localStorage.setItem(languageKey, language.value); } catch { /* storage unavailable */ }
  applyLanguage();
  line("settings-feedback", "");
  if (state) render(state);
});

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
