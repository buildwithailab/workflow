"use strict";
const $ = (s) => document.querySelector(s),
  $$ = (s) => [...document.querySelectorAll(s)];
const invoke = window.__TAURI__.core.invoke,
  listen = window.__TAURI__.event.listen;
const api = (domain, action, args = {}) =>
  invoke("dispatch", { domain, action, args });
const names = {
    codex: "Codex",
    claude: "Claude Code",
    copilot: "GitHub Copilot",
  },
  icons = { codex: "⌘", claude: "✳", copilot: "⌥" };
let db = {
    projects: [],
    sessions: [],
    active: [],
    links: [],
    jobs: [],
    preferences: {},
  },
  projectId = null,
  folderId = null,
  focusId = null,
  panel = "browser",
  providers = [],
  mode = "map",
  camera = { x: 25, y: 40, z: 0.75 },
  gesture = null,
  connecting = null,
  browserPage = null,
  browserTabs = [],
  browserJson = null,
  jsonPretty = true,
  jsonVisible = false,
  fileDirectory = "",
  fileScope = "",
  fileSelection = null,
  fileReadRevision = 0,
  changesPreference = "git",
  changesHistory = false,
  changesMode = null,
  changesScope = "",
  changeSelection = null,
  changeRevision = 0,
  historyRevision = 0,
  changeDiff = null,
  gitState = null,
  selectedCommit = null,
  baseCommit = null,
  lastTransfer = null,
  lastRestore = null,
  panelGeneration = 0,
  summaryDiffRevision = 0,
  summaryTraceRevision = 0,
  toastTimer,
  dialogSubmit = null,
  dialogCloseGuard = null;
const views = new Map(),
  activity = new Map(),
  starting = new Set(),
  gitSelected = new Set();
const projectFocus = Object.create(null);
const projectFolders = Object.create(null);
const folderLayout = Object.create(null);
const collapsedSessionFolders = new Set();
let previews = {};
const ctx = () => ({ projectId, ...(focusId ? { sessionId: focusId } : {}) });
const session = (id) => db.sessions.find((s) => s.id === id),
  project = () => db.projects.find((p) => p.id === projectId),
  active = (id) => db.active.some((a) => a.id === id);
function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}
function button(text, fn, cls = "", title = "") {
  const b = el("button", cls, text);
  b.type = "button";
  if (title) b.title = title;
  b.onclick = (e) => {
    e.stopPropagation();
    Promise.resolve(fn(e)).catch(error);
  };
  return b;
}
const workspaceMenu = el("div", "workspace-action-menu");
workspaceMenu.id = "workspace-action-menu";
workspaceMenu.setAttribute("popover", "auto");
workspaceMenu.setAttribute("role", "menu");
document.body.append(workspaceMenu);
let workspaceMenuAnchor;
function positionWorkspaceMenu() {
  if (!workspaceMenuAnchor?.isConnected) {
    workspaceMenu.hidePopover();
    return;
  }
  const anchor = workspaceMenuAnchor.getBoundingClientRect();
  const { width, height } = workspaceMenu.getBoundingClientRect();
  workspaceMenu.style.left = Math.max(8, Math.min(anchor.right - width, innerWidth - width - 8)) + "px";
  const below = anchor.bottom + 4;
  const above = anchor.top - height - 4;
  const top = below + height > innerHeight - 8 && above >= 8 ? above : below;
  workspaceMenu.style.top = Math.max(8, Math.min(top, innerHeight - height - 8)) + "px";
}
workspaceMenu.addEventListener("beforetoggle", (event) => {
  if (event.newState !== "closed") return;
  workspaceMenuAnchor?.setAttribute("aria-expanded", "false");
  workspaceMenuAnchor = null;
});
workspaceMenu.onkeydown = (event) => {
  if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
  const choices = [...workspaceMenu.querySelectorAll("button:not(:disabled)")];
  const index = choices.indexOf(document.activeElement);
  const next = event.key === "Home" ? 0 : event.key === "End" ? choices.length - 1
    : (index + (event.key === "ArrowDown" ? 1 : -1) + choices.length) % choices.length;
  event.preventDefault();
  choices[next]?.focus();
};
function workspaceMenuButton(label, actions, cls) {
  const trigger = button("⋯", () => {
    const wasOpen = workspaceMenu.matches(":popover-open") && workspaceMenuAnchor === trigger;
    workspaceMenu.hidePopover();
    if (wasOpen) return;
    const items = actions();
    if (!items.length) return;
    workspaceMenu.setAttribute("aria-label", label);
    for (const item of items) {
      item.setAttribute("role", "menuitem");
      const select = item.onclick;
      item.onclick = (event) => {
        workspaceMenu.hidePopover();
        return select(event);
      };
    }
    workspaceMenu.replaceChildren(...items);
    workspaceMenuAnchor = trigger;
    trigger.setAttribute("aria-expanded", "true");
    workspaceMenu.showPopover({ source: trigger });
    positionWorkspaceMenu();
    workspaceMenu.querySelector("button:not(:disabled)")?.focus();
  }, cls, label);
  trigger.setAttribute("aria-label", label);
  trigger.setAttribute("aria-haspopup", "menu");
  trigger.setAttribute("aria-expanded", "false");
  trigger.setAttribute("aria-controls", workspaceMenu.id);
  return trigger;
}
function error(e) {
  toast(tMessage(typeof e === "string" ? e : e?.message || String(e)), true);
}
function toast(text, bad = false) {
  clearTimeout(toastTimer);
  $("#toast span").textContent = text;
  $("#toast").classList.toggle("error", bad);
  $("#toast").hidden = false;
  toastTimer = setTimeout(
    () => ($("#toast").hidden = true),
    bad ? 18000 : 6000,
  );
}
$("#toast button").onclick = () => ($("#toast").hidden = true);
function busy(b, fn) {
  return async (...args) => {
    if (b.disabled) return;
    const text = b.textContent;
    b.disabled = true;
    b.classList.add("working");
    try {
      return await fn(...args);
    } catch (e) {
      error(e);
    } finally {
      b.disabled = false;
      b.textContent = text;
      b.classList.remove("working");
    }
  };
}
function modal(title, content, submit, label = tr("Kaydet")) {
  hideBrowser();
  $("#dialog-title").textContent = title;
  $("#dialog-body").replaceChildren(
    ...(Array.isArray(content) ? content : [content]),
  );
  $("#dialog-footer").replaceChildren();
  const close = button(tr("Vazgeç"), closeModal);
  $("#dialog-footer").append(close);
  dialogSubmit = null;
  if (submit) {
    const save = button(label, () => {}, "primary");
    save.onclick = busy(save, async () => {
      const err = $("#dialog .dialog-error");
      err?.remove();
      try {
        await submit();
        if (save.isConnected && $("#dialog").open) closeModal();
      } catch (e) {
        if (save.isConnected && $("#dialog").open)
          $("#dialog-body").append(el("p", "dialog-error", uiError(e)));
        throw e;
      }
    });
    $("#dialog-footer").append(save);
    dialogSubmit = () => save.click();
  }
  $("#dialog").showModal();
}
function closeModal() {
  if (dialogCloseGuard && !dialogCloseGuard()) return;
  $("#dialog").close();
  dialogSubmit = null;
  scheduleViewport();
}

const teamsButton = $("#teams");
const teamsEdit = $("#teams-edit");
const teamsControl = teamsButton.closest(".teams-control");
const teamsSelfLink = "https://teams.microsoft.com/l/chat/48%3Anotes/conversations";
let teamsTimer;

function paintTeams(connection) {
  teamsControl.dataset.state = connection.state;
  const label = tr({
    connected: "Bağlı",
    disconnected: "Giriş gerekli",
    "signing-in": "Giriş bekleniyor",
  }[connection.state]);
  const status = $("#teams-state");
  if (status.textContent !== label) status.textContent = label;
  const connected = connection.state === "connected";
  const signingIn = connection.state === "signing-in";
  $("#teams-action").textContent = tr(connected ? "Aç" : signingIn ? "Devam et" : "Bağlan");
  const actionLabel = tr(connected ? "Teams’i aç" : signingIn ? "Teams girişine devam et" : "Teams’e bağlan");
  teamsButton.setAttribute("aria-label", actionLabel);
  teamsButton.title = actionLabel;
  renderTeamsEditor();
}

function checkTeams(delay = 0) {
  clearTimeout(teamsTimer);
  teamsTimer = setTimeout(async () => {
    try {
      paintTeams(await api("teams", "status"));
    } catch {
      paintTeams({ state: "disconnected" });
    }
    checkTeams(teamsControl.dataset.state === "signing-in" ? 1250 : 20000);
  }, delay);
}

function teamsLink(value = "") {
  const link = el("input");
  link.type = "url";
  link.spellcheck = false;
  link.placeholder = "https://teams.microsoft.com/l/chat/…";
  link.value = value;
  return link;
}

function canSetTeamsDestination(s) {
  return !!s && !s.archived && !s.login && s.tools !== false;
}

function renderTeamsEditor() {
  teamsEdit.hidden = teamsControl.dataset.state !== "connected" ||
    !canSetTeamsDestination(session(focusId));
}

function teamsDestination(s) {
  const link = teamsLink(s.teamsChatLink || "");
  modal(tr("Teams hedefi") + " · " + s.name, [
    field(tr("Teams sohbet bağlantısı"), link),
    button(tr("Kendi sohbetim"), () => { link.value = teamsSelfLink; }, "quiet"),
    el("p", "help", tr("Bu oturumun ajanı, burada kaydettiğin sohbete mesaj gönderebilir. Erişimi kaldırmak için alanı boş bırakıp kaydet.")),
  ], () => api("teams", "set_target", { sessionId: s.id, link: link.value }));
}

teamsEdit.onclick = () => {
  const s = session(focusId);
  if (!teamsEdit.hidden && canSetTeamsDestination(s)) teamsDestination(s);
};

teamsButton.onclick = async () => {
  if (teamsButton.disabled) return;
  teamsButton.disabled = true;
  try {
    if (teamsControl.dataset.state === "connected") {
      await api("teams", "show");
    } else {
      paintTeams(await api("teams", "connect"));
    }
  } catch (e) {
    error(e);
  } finally {
    teamsButton.disabled = false;
    checkTeams(1000);
  }
};

$("#dialog-close").onclick = closeModal;
$("#dialog").addEventListener("close", scheduleViewport);
$("#dialog").addEventListener("cancel", (e) => {
  if (!dialogCloseGuard) return;
  e.preventDefault();
  closeModal();
});
$("#dialog").addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) &&
      (e.key === "Enter" || (dialogCloseGuard && e.key.toLowerCase() === "s"))) {
    e.preventDefault();
    dialogSubmit?.();
  }
});
function field(label, input) {
  const box = el("div", "form-field"),
    caption = el("label", "", label);
  const control = input.matches("input, select, textarea")
    ? input
    : input.querySelector("input, select, textarea");
  if (control) {
    control.id ||= "field-" + crypto.randomUUID();
    caption.htmlFor = control.id;
  }
  box.append(caption, input);
  return box;
}
function input(value = "", placeholder = "") {
  const n = el("input");
  n.value = value;
  n.placeholder = placeholder;
  return n;
}
function check(text, checked = false) {
  const label = el("label", "check");
  const n = el("input");
  n.type = "checkbox";
  n.checked = checked;
  label.append(n, document.createTextNode(text));
  return { label, input: n };
}
function select(options, value) {
  const n = el("select");
  for (const o of options) n.add(new Option(o.label || o.name || o.id, o.id));
  if (value) n.value = value;
  return n;
}
async function chooseFolder() {
  return window.__TAURI__.core.invoke("plugin:dialog|open", {
    options: { directory: true, multiple: false },
  });
}
async function sync() {
  db = await api("app", "state");
  render();
  return db;
}
function visibleSessions() {
  return projectSessions().filter((s) => (s.folderId || null) === folderId);
}
function projectSessions() {
  return db.sessions.filter((s) => s.projectId === projectId && !s.archived);
}
function foldersFor(id) {
  return db.projects.find((p) => p.id === id)?.sessionFolders || [];
}
function knownFolder(pid, id) {
  return foldersFor(pid).some((folder) => folder.id === id) ? id : null;
}
function folderOptions(pid) {
  return [{ id: "", label: tr("Genel") },
    ...foldersFor(pid).map((folder) => ({ id: folder.id, label: folder.name }))];
}
function sidebarFolders(pid) {
  const visible = new Set();
  const archived = new Set();
  for (const s of db.sessions) {
    if (s.projectId === pid) (s.archived ? archived : visible).add(s.folderId || "");
  }
  return folderOptions(pid).filter((folder) => visible.has(folder.id) || !archived.has(folder.id));
}
function folderLayoutKey(project = projectId, folder = folderId) {
  return JSON.stringify([project, folder]);
}
function status(id) {
  if (starting.has(id)) return { key: "working", label: tr("Açılıyor…") };
  if (!active(id)) return { key: "stopped", label: tr("Kapalı") };
  return activity.get(id) || { key: "running", label: tr("Açık") };
}
function stateBadge(id) {
  const s = status(id),
    n = el("span", "state");
  n.dataset.state = s.key;
  n.append(el("i", "dot"), el("span", "", s.label));
  return n;
}
function selectProject(id) {
  if (!db.projects.some((p) => p.id === id && !p.removed)) return;
  selectScope(id, projectFolders[id]);
  render();
  refreshPanel();
  remember();
}
function selectScope(pid, requestedFolder) {
  const next = knownFolder(pid, requestedFolder);
  if (projectId === pid && folderId === next) return;
  if (projectId) folderLayout[folderLayoutKey()] = { focusId, camera };
  projectId = pid;
  folderId = next;
  const saved = folderLayout[folderLayoutKey()];
  focusId = saved?.focusId || projectFocus[pid] || null;
  camera = saved?.camera || { x: 25, y: 40, z: 0.75 };
  fileDirectory = "";
  browserPage = null;
  applyCamera();
}
function selectFolder(id) {
  selectScope(projectId, id);
  render();
  remember();
  refreshPanel().catch(error);
}
function clickSessionFolder(id) {
  const key = folderLayoutKey(projectId, id);
  if (id !== folderId) {
    collapsedSessionFolders.delete(key);
    selectFolder(id);
    return;
  }
  if (!collapsedSessionFolders.delete(key)) collapsedSessionFolders.add(key);
  renderSessionList();
  remember();
}
function selectSession(id, center = false, tab = null) {
  const s = session(id);
  if (!s || s.archived) return;
  selectScope(s.projectId, s.folderId);
  focusId = id;
  render();
  if (tab) openPanel(tab).catch(error);
  else refreshPanel();
  remember();
  if (center && mode === "map") centerCard();
  if (center && mode === "grid")
    views.get(id)?.card.scrollIntoView({ block: "nearest", inline: "nearest" });
}
function render() {
  db.projects ??= [];
  db.sessions ??= [];
  db.links ??= [];
  db.jobs ??= [];
  db.active ??= [];
  if (!project() || project()?.removed)
    selectScope(db.projects.find((p) => !p.removed)?.id || null, null);
  if (folderId !== knownFolder(projectId, folderId)) selectScope(projectId, null);
  const folders = sidebarFolders(projectId);
  if (folders.length && !folders.some((folder) => folder.id === (folderId || ""))) {
    selectScope(projectId, folders[0].id);
  }
  const sessions = visibleSessions();
  if (
    !sessions.some((s) => s.id === focusId)
  )
    focusId = sessions[0]?.id || null;
  renderTeamsEditor();
  renderProjectRail();
  renderSessionList();
  renderSessionTabs();
  renderCards();
  renderProviderCards();
  $("#project-title").textContent = project()?.name || tr("Çalışma alanı");
  $("#project-title").title = $("#project-title").textContent;
  const count = sessions.length;
  $("#session-count").textContent = countLabel(count, "{count} oturum");
  $("#panel-context").textContent =
    (project()?.name || "") +
    (session(focusId) ? " · " + session(focusId).name : "");
  const running = sessions.filter((s) => active(s.id)).length;
  $("#running-status span").textContent = running
    ? countLabel(running, "{count} oturum çalışıyor")
    : tr("Çalışan oturum yok");
  $("#empty").hidden = sessions.length > 0;
  if (!sessions.length) {
    $("#empty h2").textContent = tr(projectId ? "Bu klasörde henüz oturum yok." : "Bir sonraki işine yer aç.");
    $("#empty p").textContent = tr(projectId
      ? "Bu klasörde çalışmaya başlamak için yeni bir oturum aç."
      : "Proje ekle, bir sağlayıcı seç ve ilk oturumunu başlat.");
  }
  $("#empty").style.marginTop = "100px";
  const options = sessions.map((s) => ({
    id: s.id,
    label: names[s.provider] + " · " + s.name,
  }));
  const bs = $("#browser-session");
  bs.replaceChildren(...options.map((o) => new Option(o.label, o.id)));
  bs.value = focusId || "";
  paintBrowserPermission();
  if (panel === "links") renderLinks();
  renderChatView();
  scheduleNotificationContext();
}
function projectInitials(name) {
  const words = name.match(/[\p{L}\p{N}][\p{L}\p{N}\p{M}]*/gu) || [];
  const initials = words.length > 1
    ? words.slice(0, 2).map((word) => [...word][0]).join("")
    : words[0] || "·";
  return [...initials.toLocaleUpperCase(uiLocale)].slice(0, 2).join("");
}
function renderProjectRail() {
  const root = $("#project-switcher");
  const focused = root.contains(document.activeElement) ? document.activeElement.dataset.projectId : null;
  const activeIds = new Set(db.active.map((run) => run.id));
  const running = new Set(db.sessions.filter((s) => !s.archived && activeIds.has(s.id)).map((s) => s.projectId));
  const entries = db.projects.filter((p) => !p.removed).map((p) => {
    const live = running.has(p.id);
    const label = live ? p.name + " · " + tr("Açık oturum var") : p.name;
    const b = button(projectInitials(p.name), () => selectProject(p.id), "icon rail-project", label);
    b.dataset.projectId = p.id;
    b.dataset.color = String([...p.id].reduce((sum, ch) => sum + ch.charCodeAt(0), 0) % 5);
    b.dataset.running = String(live);
    const attention = notificationProjectBadge(p.id);
    if (attention) {
      b.append(attention.badge);
      b.title = label + " · " + attention.label;
    }
    b.setAttribute("aria-label", b.title || label);
    b.setAttribute("aria-pressed", String(p.id === projectId));
    return b;
  });
  root.replaceChildren(...entries);
  entries.find((b) => b.dataset.projectId === focused)?.focus({ preventScroll: true });
  if (root.dataset.selected !== projectId) {
    root.dataset.selected = projectId || "";
    entries.find((b) => b.dataset.projectId === projectId)?.scrollIntoView({ block: "nearest" });
  }
}
function renderSessionList() {
  const root = $("#project-sessions");
  if (workspaceMenuAnchor && root.contains(workspaceMenuAnchor)) workspaceMenu.hidePopover();
  const focused = root.contains(document.activeElement) ? document.activeElement : null;
  const hasProject = !!project();
  $("#project-rules").hidden = $("#remove-project").hidden = $("#add-session-folder").hidden = !hasProject;
  if (!hasProject) {
    root.replaceChildren(el("p", "help", tr("Bir proje seç.")));
    return;
  }
  const sessions = projectSessions();
  const entries = collapsedSessionFolders.has(folderLayoutKey()) ? [] : visibleSessions().map((s) => {
    const b = button("", () => selectSession(s.id), "session-entry" + (s.id === focusId ? " active" : ""), s.name);
    b.dataset.sessionId = s.id;
    b.setAttribute("aria-pressed", String(s.id === focusId));
    b.append(
      el("span", "provider-icon " + s.provider, icons[s.provider]),
      el("span", "name", s.name),
      stateBadge(s.id),
    );
    b.ondblclick = () => rename(s.id);
    const row = el("div", "session-entry-row");
    const options = workspaceMenuButton(tr("Oturum seçenekleri"), () => sessionMenu(s.id), "icon quiet session-entry-options");
    options.dataset.sessionOptions = s.id;
    row.append(b, options);
    return row;
  });
  root.replaceChildren(...sidebarFolders(projectId).map((folder) => {
    const key = folder.id || null;
    const count = sessions.filter((session) => (session.folderId || null) === key).length;
    const selected = key === folderId;
    const expanded = selected && !collapsedSessionFolders.has(folderLayoutKey(projectId, key));
    const group = el("section", "session-folder");
    const row = el("div", "session-folder-row");
    const children = el("div", "session-folder-children");
    children.id = "session-folder-children-" + folder.id;
    const choose = button("", () => clickSessionFolder(key), "session-folder-select", folder.label);
    choose.dataset.folderId = folder.id;
    choose.setAttribute("aria-expanded", String(expanded));
    choose.setAttribute("aria-controls", children.id);
    if (selected) choose.setAttribute("aria-current", "true");
    const chevron = el("span", "session-folder-chevron");
    chevron.setAttribute("aria-hidden", "true");
    choose.append(
      chevron,
      el("span", "session-folder-icon"),
      el("span", "name", folder.label),
      el("span", "folder-count", count),
    );
    row.append(choose);
    if (selected && (count || key)) {
      if (key) {
        const menu = workspaceMenuButton(tr("Klasör ayarları"), () => folderMenu(key, folder.label), "icon quiet");
        menu.dataset.folderMenu = folder.id;
        row.append(menu);
      } else {
        const label = tr("Klasörü arşive taşı");
        const archive = button("", () => {}, "icon quiet folder-archive-button", label);
        archive.setAttribute("aria-label", label);
        archive.onclick = busy(archive, () => archiveFolder(folder.label, folderMembers(null)));
        row.append(archive);
      }
    }
    children.hidden = !expanded;
    if (expanded)
      children.append(...(entries.length ? entries : [el("p", "help", tr("Henüz oturum yok."))]));
    group.append(row, children);
    return group;
  }));
  if (focused) {
    const { sessionId, sessionOptions, folderId, folderMenu } = focused.dataset;
    [...root.querySelectorAll("button")].find((b) =>
      (sessionId !== undefined && b.dataset.sessionId === sessionId) ||
      (sessionOptions !== undefined && b.dataset.sessionOptions === sessionOptions) ||
      (folderId !== undefined && b.dataset.folderId === folderId) ||
      (folderMenu !== undefined && b.dataset.folderMenu === folderMenu),
    )?.focus({ preventScroll: true });
  }
}

function renderSessionTabs() {
  const root = $("#session-tabs");
  const previousFocus = root.contains(document.activeElement) ? document.activeElement.dataset.sessionId : null;
  const tabs = visibleSessions().map((s) => {
    const selected = s.id === focusId;
    const tab = button("", () => selectSession(s.id, true), "session-tab", s.name);
    tab.id = "session-tab-" + s.id;
    tab.dataset.sessionId = s.id;
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-selected", String(selected));
    tab.setAttribute("aria-controls", "viewport");
    tab.tabIndex = selected ? 0 : -1;
    tab.append(el("span", "provider-icon " + s.provider, icons[s.provider]),
      el("span", "name", s.name), stateBadge(s.id));
    return tab;
  });
  $("#session-tabbar").hidden = !tabs.length;
  root.replaceChildren(...tabs);
  const selected = tabs.find((b) => b.dataset.sessionId === focusId);
  const viewport = $("#viewport");
  if (selected) {
    viewport.setAttribute("role", "tabpanel");
    viewport.setAttribute("aria-labelledby", selected.id);
    if (root.dataset.selected !== focusId) selected.scrollIntoView({ block: "nearest", inline: "nearest" });
    if (previousFocus) selected.focus({ preventScroll: true });
  } else {
    viewport.removeAttribute("role");
    viewport.removeAttribute("aria-labelledby");
  }
  root.dataset.selected = focusId || "";
}

$("#session-tabs").onkeydown = (event) => {
  const keys = ["ArrowLeft", "ArrowRight", "Home", "End"];
  if (!keys.includes(event.key)) return;
  const tabs = [...$("#session-tabs").children];
  const index = tabs.indexOf(event.target);
  if (index < 0) return;
  event.preventDefault();
  const target = event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1
    : (index + tabs.length + (event.key === "ArrowLeft" ? -1 : 1)) % tabs.length;
  selectSession(tabs[target].dataset.sessionId, true);
};

function folderNameDialog(key = null, name = "") {
  if (!projectId) return;
  const pid = projectId;
  const fieldName = input(name);
  fieldName.maxLength = 80;
  fieldName.required = true;
  modal(tr(key ? "Klasörü yeniden adlandır" : "Oturum klasörü ekle"),
    field(tr("Klasör adı"), fieldName), async () => {
      const folder = await api("app", key ? "folder_rename" : "folder_create",
        { projectId: pid, folderId: key, name: fieldName.value });
      await sync();
      if (pid === projectId) selectFolder(folder.id);
    }, tr(key ? "Kaydet" : "Klasörü oluştur"));
  fieldName.focus();
}

function folderMembers(key) {
  return projectSessions().filter((s) => (s.folderId || null) === key);
}

function folderMenu(key, name) {
  const pid = projectId;
  const members = folderMembers(key);
  return [
    button(tr("Klasörü yeniden adlandır"), () => folderNameDialog(key, name)),
    ...(members.length ? [button(tr("Klasörü arşive taşı"), () => archiveFolder(name, members))] : []),
    button(tr("Klasörü kaldır"), () => modal(tr("Klasörü kaldır"),
      el("p", "help", tr("Oturumlar Genel klasörüne taşınacak.")), async () => {
        await api("app", "folder_remove", { projectId: pid, folderId: key });
        collapsedSessionFolders.delete(folderLayoutKey(pid, key));
        await sync();
        if (pid === projectId) selectFolder(null);
      }, tr("Kaldır"))),
  ];
}

async function archiveFolder(name, members) {
  if (!members.length) return;
  const state = await api("app", "state");
  const runs = await Promise.all(members.map((member) => archiveProcesses(member, state)));
  if (runs.some(({ terminal, preview }) => terminal || preview)) {
    folderArchiveDialog(name, members, true);
  } else {
    await applyFolderArchive(members, true);
  }
}

function folderArchiveDialog(name, members, archive) {
  const title = tr(archive ? "Klasörü arşive taşı" : "Klasörü geri getir");
  const description = archive
    ? tr("Bu klasördeki {count} oturum arşivlenecek. Çalışan terminaller ve önizlemeler durdurulacak. Workflow’un oluşturduğu çalışma klasörleri de taşınacak.", { count: members.length })
    : tr("Bu klasördeki {count} oturum kendi projesine geri getirilecek.", { count: members.length });
  modal(title + " · " + name, el("p", "help", description),
    () => applyFolderArchive(members, archive, archive), title);
}

async function applyFolderArchive(members, archive, stop = false) {
  const failures = [];
  let changed = 0;
  const owner = db.projects.find((p) => p.id === members[0].projectId);
  if (!archive && owner?.removed) await api("app", "add_project", { directory: owner.directory });
  for (const member of members) {
    try {
      const state = await api("app", "state");
      const current = state.sessions.find((s) => s.id === member.id && s.projectId === member.projectId
        && (s.folderId || null) === (member.folderId || null));
      if (!current) throw Error(tr("Oturum artık bu klasörde değil."));
      if (!!current.archived === archive) continue;
      if (stop) {
        const { terminal, preview } = await archiveProcesses(current, state);
        await stopArchiveProcesses(current.id, terminal, preview);
      }
      await api("app", "session_patch", { id: current.id, archived: archive });
      changed++;
    } catch (err) {
      failures.push(member.name + " · " + uiError(err));
    }
  }
  await sync();
  refreshPanel().catch(error);
  if (failures.length) {
    const summary = tr("{count} oturum taşındı. Şu oturumlar taşınamadı.", { count: changed });
    throw Error([summary, ...failures].join("\n"));
  }
  if (!archive) selectSession(members[0].id);
  toast(tr(archive ? "Klasördeki oturumlar arşive taşındı." : "Klasördeki oturumlar geri getirildi."));
}
$("#add-session-folder").onclick = () => folderNameDialog();

function moveSessionDialog(s) {
  const destination = select(folderOptions(s.projectId), s.folderId || "");
  modal(tr("Klasöre taşı"), field(tr("Oturum klasörü"), destination), async () => {
    await api("app", "folder_move_session", {
      projectId: s.projectId, id: s.id, folderId: destination.value || null,
    });
    await sync();
    selectSession(s.id, true);
  }, tr("Taşı"));
}
$("#project-rules").onclick = () => {
  const p = project();
  if (p) editProjectInstructions(p).catch(error);
};
$("#remove-project").onclick = () => {
  const p = project();
  if (!p) return;
  modal(
    tr("Projeyi listeden kaldır"),
    el("p", "help", p.name + tr(" listeden kaldırılacak. Proje dosyaları korunur. Açık oturumlarını önce durdur.")),
    async () => {
      await api("app", "remove_project", { id: p.id });
      if (projectId === p.id) projectId = null;
      await sync();
    },
    tr("Kaldır"),
  );
};
async function editProjectInstructions(project) {
  const saved = await api("app", "project_instructions_read", { projectId: project.id });
  const editor = el("textarea");
  editor.value = saved.text;
  editor.rows = 12;
  editor.spellcheck = false;
  editor.maxLength = saved.maxBytes;
  const note = el("p", "help", tr("Bu talimatlar Workflow’da saklanır ve Codex, Claude Code ve Copilot oturumlarına bir sonraki başlatmada uygulanır."));
  modal(tr("Proje talimatları") + " · " + project.name, [
    note, field(tr("Ortak talimatlar"), editor),
  ], async () => {
    await api("app", "project_instructions_save", {
      projectId: project.id, revision: saved.revision, text: editor.value,
    });
    toast(tr("Proje talimatları kaydedildi."));
  });
  editor.focus();
}
async function rename(id) {
  const s = session(id),
    name = input(s.name);
  modal(tr("Oturumun adı"), field(tr("Ad"), name), async () => {
    if (name.value.trim().length > 100) throw Error(tr("En fazla 100 karakter"));
    await api("app", "session_patch", {
      id,
      name: name.value.trim() || tr("Oturum"),
    });
    await sync();
  });
  name.select();
}
const usageInFlight = new Map();
const defaultAccount = "default";
const accountLabel = (account) =>
  account.id === defaultAccount ? tr("Varsayılan hesap") : account.name;
const providerInfo = (id) => providers.find((p) => p.id === id);
const providerAccount = (id, accountId = defaultAccount) =>
  providerInfo(id)?.accounts.find((account) => account.id === accountId);
const usageKey = (provider, accountId) => provider + ":" + accountId;
function usageRow(w) {
  const row = el("div", "usage-window"),
    line = el("div", "usage-line");
  row.dataset.window = w.id;
  const windowLabel = usageLabel(w.label);
  line.append(el("span", "", windowLabel));
  if (w.unlimited) {
    line.append(el("strong", "usage-left", tr("Sınırsız")));
    row.append(line);
    return row;
  }
  const left = Math.max(0, Math.min(100, Number(w.remainingPercent)));
  const label = new Intl.NumberFormat(uiLocale, {
    style: "percent",
    maximumFractionDigits: 1,
  }).format(left / 100);
  line.append(el("strong", "usage-left", label + tr(" kaldı")));
  const meter = el("div", "usage-meter"),
    fill = el("i");
  fill.style.width = left + "%";
  if (left <= 10) fill.className = "critical";
  else if (left <= 25) fill.className = "low";
  meter.setAttribute("role", "progressbar");
  meter.setAttribute("aria-label", windowLabel + tr(" kalan kullanım"));
  meter.setAttribute("aria-valuemin", "0");
  meter.setAttribute("aria-valuemax", "100");
  meter.setAttribute("aria-valuenow", String(left));
  meter.append(fill);
  row.append(line, meter);
  if (w.resetsAt) {
    const date = new Date(w.resetsAt);
    if (!Number.isNaN(date.getTime()))
      row.title = tr("Yenilenme: ") + date.toLocaleString(uiLocale);
  }
  return row;
}
function renderProviderCards() {
  const box = $("#provider-cards");
  box.replaceChildren();
  const count = providers.filter((p) => p.installed && p.accounts.some(isConnected)).length + "/" + Object.keys(names).length;
  $("#provider-count").textContent = count;
  $("#show-providers").setAttribute("aria-label", tr("Sağlayıcılar") + " " + count);
  for (const id of Object.keys(names)) {
    const p = providerInfo(id),
      card = el("div", "provider-card"),
      row = el("div", "row");
    card.dataset.provider = id;
    const refresh = button(
      "↻",
      () => Promise.allSettled(p.accounts.map((a) => refreshProviderUsage(id, a.id))),
      "icon quiet",
      tr("Kullanımı yenile"),
    );
    refresh.disabled = !p?.installed ||
      p.accounts.some((a) => usageInFlight.has(usageKey(id, a.id)));
    row.append(
      el("span", "provider-icon " + id, icons[id]),
      el("strong", "grow", names[id]),
      el("small", "", p?.installed ? tr("● Kurulu") : tr("○ Kurulu değil")),
      refresh,
    );
    const usageList = el("div", "provider-account-list");
    const accounts = p?.accounts || [];
    for (const account of accounts) {
      const section = el("section", "provider-account");
      section.dataset.accountId = account.id;
      if (accounts.length > 1) section.append(el("strong", "provider-account-name", accountLabel(account)));
      section.append(renderAccountUsage(p, account));
      usageList.append(section);
    }
    if (!accounts.length) usageList.append(el("p", "usage-message", tr("Kullanım alınıyor…")));
    const actions = el("div", "row provider-actions");
    if (p && !p.installed) actions.append(button(tr("Kur"), () => openProviderSetup(1, id)));
    const data = providerAccount(id)?.usage;
    if (data?.connection === "connected") {
      actions.append(el("span", "provider-connected", tr("✓ Hesap bağlı")));
    } else if (p?.installed && data && !usageInFlight.has(usageKey(id, defaultAccount))) {
      const label =
        data.connection === "expired"
          ? tr("Girişi yenile")
          : data.connection === "signed-out"
            ? tr("Hesabı bağla")
            : tr("Hesap girişini aç");
      actions.append(button(label, () => openProviderLogin(id)));
    }
    const manage = button(tr("Hesaplar"), () => openProviderAccounts(id));
    manage.disabled = !p?.installed;
    actions.append(
      manage,
      button(
        tr("Program seç"),
        async () => {
          const selected = await invoke("plugin:dialog|open", {
            options: { multiple: false, directory: false },
          });
          if (selected) {
            await api("app", "provider_path", { provider: id, path: selected });
            await scanProviders();
          }
        },
        "quiet",
      ),
    );
    card.append(row, usageList, actions);
    box.append(card);
  }
}
const isConnected = (account) =>
  account.id === defaultAccount ? account.usage?.connection === "connected"
    : account.usage?.connection === "connected" || (account.verified && !account.usage?.connection);

async function openProviderSetup(step = 0, selectedProvider = null) {
  const steps = [tr("Algıla"), tr("Kur"), tr("Giriş yap")];
  const options = step === 1 ? await api("app", "provider_install_options") : null;
  const content = el("div", "setup-content");
  const progress = el("ol", "setup-steps");
  steps.forEach((label, i) => {
    const item = el("li", "", label);
    if (i === step) item.setAttribute("aria-current", "step");
    progress.append(item);
  });
  content.append(progress);
  const list = el("div", "setup-list");
  const installed = (id) => providerInfo(id)?.installed;
  for (const id of selectedProvider ? [selectedProvider] : Object.keys(names)) {
    const row = el("section", "setup-row");
    const header = el("div", "row");
    header.append(el("strong", "grow", names[id]),
      el("small", "", installed(id) ? tr("● Kurulu") : tr("○ Kurulu değil")));
    row.append(header);
    if (step === 1 && !installed(id)) {
      for (const option of options[id]) {
        const command = el("div", "row setup-command");
        const install = button(tr("Kur"), () => {
          const confirmation = el("div", "setup-content");
          confirmation.append(el("p", "", tr("Bu komut bilgisayarına {provider} kurar.", { provider: names[id] })),
            el("code", "", option.command));
          modal(tr("Kurulumu onayla"), confirmation, async () => {
            await api("app", "provider_install", { provider: id, method: option.id, confirmed: true });
            await scanProviders();
            $("#dialog").addEventListener("close", () => openProviderSetup(1, selectedProvider).catch(error), { once: true });
          }, tr("Kur"));
        });
        install.disabled = !option.available;
        command.append(el("code", "grow", option.command), install);
        row.append(command);
        if (!option.available) row.append(el("small", "", tr(option.id === "npm" ? "npm ve Node.js 22+ gerekli." : "WinGet bulunamadı.")));
      }
    }
    if (step === 2 && installed(id)) {
      if (providerInfo(id).accounts.some(isConnected)) row.append(el("small", "provider-connected", tr("✓ Hesap bağlı")));
      else row.append(button(tr("Hesabı bağla"), async () => {
        await openProviderLogin(id);
        if ($("#dialog").open)
          $("#dialog").addEventListener("close", () => openProviderSetup(2).catch(error), { once: true });
      }));
    }
    list.append(row);
  }
  const actions = el("div", "row");
  const rescan = button(tr("Yeniden tara"), () => {});
  rescan.onclick = busy(rescan, async () => {
    await scanProviders();
    if (content.isConnected && $("#dialog").open) await openProviderSetup(step, selectedProvider);
  });
  actions.append(rescan);
  if (step === 1 && selectedProvider)
    actions.append(button(tr("Tüm sağlayıcılar"), () => openProviderSetup(1)));
  if (step > 0) actions.append(button(tr("Geri"), () => openProviderSetup(step - 1)));
  content.append(list, actions);
  modal(tr("Sağlayıcı kurulumu"), content, async () => {
    if (step < 2)
      $("#dialog").addEventListener("close", () => openProviderSetup(step + 1).catch(error), { once: true });
  }, step === 2 ? tr("Bitir") : tr("Sonraki"));
  const skip = $("#dialog-footer button");
  skip.textContent = tr("Atla");
}
function renderAccountUsage(provider, account) {
    const usage = el("div", "provider-usage"), data = account.usage;
    usage.setAttribute("aria-busy", String(usageInFlight.has(usageKey(provider.id, account.id))));
    if (data?.state === "ready" && data.windows?.length) {
      data.windows.slice(0, 2).forEach((w) => usage.append(usageRow(w)));
      if (data.windows.length > 2) {
        const extra = el("details", "usage-extra");
        extra.append(
          el("summary", "", tr("Diğer limitler · ") + (data.windows.length - 2)),
        );
        data.windows.slice(2).forEach((w) => extra.append(usageRow(w)));
        usage.append(extra);
      }
      usage.title =
        tr("Son güncelleme: ") +
        new Date(data.updatedAt).toLocaleTimeString(uiLocale);
    } else
      usage.append(
        el(
          "span",
          "usage-message",
          !provider.installed
            ? tr("Kullanım için CLI kurulumu gerekli.")
            : tMessage(data?.message) || (account.verified ? tr("Kullanım alınıyor…") : tr("Giriş bekleniyor.")),
        ),
      );
    return usage;
}
async function refreshProviderUsage(id, accountId = defaultAccount) {
  const p = providerInfo(id);
  if (!p?.installed || !providerAccount(id, accountId)) return;
  const key = usageKey(id, accountId);
  if (usageInFlight.has(key)) return usageInFlight.get(key);
  const executable = p.executable;
  const request = api("app", "provider_usage", { provider: id, accountId });
  usageInFlight.set(key, request);
  renderProviderCards();
  try {
    const result = await request;
    const current = providerAccount(id, accountId);
    if (current && providerInfo(id)?.executable === executable) current.usage = result;
  } catch {
    const current = providerAccount(id, accountId);
    if (current && providerInfo(id)?.executable === executable)
      current.usage = {
        state: "unavailable",
        message: tr("Kullanım alınamadı. Yeniden dene."),
      };
  } finally {
    usageInFlight.delete(key);
    renderProviderCards();
    if (
      providerInfo(id)?.executable !== executable &&
      !$("#providers-section").hidden
    )
      refreshProviderUsage(id, accountId);
  }
}
async function refreshProviderUsageAll() {
  if (!$("#providers-section").hidden)
    await Promise.allSettled(providers.flatMap((p) =>
      p.accounts.map((account) => refreshProviderUsage(p.id, account.id)),
    ));
}
async function scanProviders() {
  const found = await api("app", "providers");
  providers = found.map((p) => ({
    ...p,
    accounts: p.accounts.map((account) => ({
      ...account,
      usage: providerInfo(p.id)?.executable === p.executable
        ? providerAccount(p.id, account.id)?.usage
        : undefined,
    })),
  }));
  renderProviderCards();
  renderCards();
  await refreshProviderUsageAll();
}
function renderCards() {
  const ids = new Set(visibleSessions().map((s) => s.id));
  for (const [id, v] of views) {
    if (!db.sessions.some((s) => s.id === id && !s.archived)) {
      v.observer.disconnect();
      v.term.dispose();
      v.card.remove();
      views.delete(id);
    } else v.card.hidden = !ids.has(id);
  }
  visibleSessions().forEach((s, index) => {
    let v = views.get(s.id);
    if (!v) {
      v = createCard(s, index);
      views.set(s.id, v);
      $("#cards").append(v.card);
      attach(v, s).catch(error);
    }
    v.card.hidden = false;
    v.card.classList.toggle("focused", s.id === focusId);
    v.title.textContent = s.name;
    v.title.title = s.name;
    const st = status(s.id);
    v.state.replaceWith((v.state = stateBadge(s.id)));
    v.control.textContent = starting.has(s.id)
      ? "◌"
      : active(s.id)
        ? "■"
        : "▶";
    v.control.title = active(s.id)
      ? tr("Oturumu durdur")
      : s.login
        ? tr("Hesap girişini başlat")
        : tr("Aynı konuşmaya devam et");
    v.control.disabled = starting.has(s.id);
    v.model.textContent = v.observedModel || s.model || tr("Model seç⌄");
    const agentCount = active(s.id) && !s.login ? 1 + v.childAgents.size : 0;
    v.agentBadge.textContent = countLabel(agentCount, "{count} ajan");
    v.model.disabled = s.login === true;
    v.model.title = s.login ? tr("Hesap girişinde model kullanılmaz") : tr("Model seç");
    v.model.hidden = s.login === true;
    paintPreview(s.id, v);
    v.summary.hidden = s.login === true;
    v.notice.hidden = !s.login;
    if (s.login) {
      v.notice.replaceChildren(
        el(
          "span",
          "",
          active(s.id)
            ? tr("Hesap girişi açık. Terminaldeki adımları tamamla.")
            : tr("Bu kart hesap girişi içindir. Projede çalışmak için konuşmaya geç."),
        ),
      );
      if (!active(s.id))
        v.notice.append(
          button(tr("Konuşmaya geç"), () => conversationFromLogin(s.id), "primary"),
        );
    }
    v.term.options.disableStdin = !active(s.id);
    v.pathMode.textContent =
      (s.workspaceKind === "direct"
        ? tr("Mevcut klasör")
        : s.workspaceKind === "worktree"
          ? "Git worktree"
          : tr("Ayrı klasör")) +
      " · ";
    v.pathValue.textContent = s.directory;
    v.path.title = v.pathMode.textContent + s.directory;
    const account = providerAccount(s.provider, s.accountId || defaultAccount);
    v.account.hidden =
      (!s.accountId || s.accountId === defaultAccount) &&
      (providerInfo(s.provider)?.accounts.length || 0) < 2;
    v.account.textContent = account ? accountLabel(account) : tr("Kaldırılmış hesap");
    v.account.title = tr("Sağlayıcı hesabı · ") + v.account.textContent;
    v.card.style.left = (s.position?.x ?? 40 + (index % 2) * 780) + "px";
    v.card.style.top =
      (s.position?.y ?? 50 + Math.floor(index / 2) * 580) + "px";
    v.card.style.width = (s.size?.width || 720) + "px";
    v.card.style.height = (s.size?.height || 500) + "px";
    v.overlay.hidden = active(s.id) || v.hasOutput;
    v.overlayText.textContent =
      tr("Bu oturum kapalı. Üstteki ▶ ile aynı konuşmaya devam et.");
  });
  drawEdges();
  requestAnimationFrame(fitTerminals);
}
function terminalClipboard(term, host, writableRun, onInput, sessionId) {
  let inputQueue = Promise.resolve(), pasting = false, pastedInput;
  let inputGeneration = 0, pendingInput = 0;
  const enqueue = (action, affectsDraft = true) => {
    if (affectsDraft) pendingInput++;
    inputQueue = inputQueue.then(action).catch(error).finally(() => {
      if (affectsDraft) pendingInput--;
    });
    return inputQueue;
  };
  term.onData((data) => {
    if (pasting) {
      pastedInput = onInput(data);
      return;
    }
    const run = writableRun();
    const generation = inputGeneration;
    // xterm reports focus changes when the user clicks an action outside the terminal.
    // Keep those reports ordered without treating them as pending draft text.
    const focusReport = data === "\x1b[I" || data === "\x1b[O";
    enqueue(() => {
      if (generation === inputGeneration && run && host.isConnected && writableRun() === run)
        return onInput(data);
    }, !focusReport);
  });
  const copy = async () => {
    const text = term.getSelection();
    if (!text) throw Error(tr("Kopyalamak için terminalde metin seç."));
    await api("clipboard", "write", { text });
  };
  const readClipboard = async () => {
    const attachment = sessionId && await api("clipboard", "read_attachment", { id: sessionId });
    return attachment || api("clipboard", "read");
  };
  const paste = (readPayload = readClipboard) => {
    const run = writableRun();
    if (!run || term.options.disableStdin) {
      error(tr("Yapıştırmak için oturumun terminalini başlat."));
      return;
    }
    const generation = inputGeneration;
    return enqueue(async () => {
      const canPaste = () => host.isConnected && writableRun() === run && !term.options.disableStdin;
      if (generation !== inputGeneration || !canPaste()) return;
      const payload = await readPayload();
      const attachment = typeof payload === "object" ? payload : null;
      // A stopped, replaced or detached terminal must not receive a pending paste.
      if (!canPaste()) {
        await discardScreenshot(sessionId, attachment);
        return;
      }
      let text = attachment ? " " + imagePrompt([attachment], session(sessionId)?.provider) + " " : payload;
      if (!text) throw Error(tr("Windows panosundan yapıştırılabilir metin alınamadı."));
      if (/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]/.test(text)) {
        inputGeneration++;
        throw Error(tr("Pano metninde terminal denetim karakterleri var. Metni temizleyip tekrar dene."));
      }
      const bracketed = term.modes.bracketedPasteMode && !term.options.ignoreBracketedPasteMode;
      if (!bracketed && /[\r\n\t]/.test(text)) {
        // Discard keystrokes buffered before the user makes a decision, including Enter.
        inputGeneration++;
        if (!(await confirmTerminalPaste()) || !canPaste()) return;
        text = text.replace(/\r\n|[\r\n\t]/g, " ");
      }
      pasting = true;
      try {
        pastedInput = undefined;
        term.paste(text);
      } finally {
        pasting = false;
      }
      await pastedInput;
    });
  };
  host.addEventListener("paste", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const image = sessionId && [...(e.clipboardData?.files || [])].find(file => file.type.startsWith("image/"));
    if (image) {
      paste(() => saveClipboardImage(sessionId, image));
      return;
    }
    paste(() => e.clipboardData?.getData("text/plain") ?? "");
  }, true);
  term.attachCustomKeyEventHandler((e) => {
    const key = e.key.toLowerCase();
    const modifier = (e.ctrlKey || e.metaKey) && !e.altKey;
    const copyKey = modifier &&
      ((key === "c" && (e.shiftKey || term.hasSelection())) ||
       (key === "insert" && !e.shiftKey));
    const pasteKey = (modifier && key === "v") ||
      (key === "insert" && e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey);
    if (copyKey || pasteKey) {
      e.preventDefault();
      if (e.type === "keydown") Promise.resolve(copyKey ? copy() : paste()).catch(error);
      return false;
    }
    if (e.type === "keydown" && e.shiftKey && ["PageUp", "PageDown"].includes(e.key)) {
      e.preventDefault();
      term.scrollPages(e.key === "PageUp" ? -1 : 1);
      return false;
    }
    return true;
  });
  const menu = el("div", "terminal-menu");
  menu.setAttribute("popover", "auto");
  menu.setAttribute("role", "menu");
  menu.setAttribute("aria-label", tr("Terminal"));
  const item = (label, shortcut, action) => {
    const b = button("", () => {
      menu.hidePopover();
      term.focus();
      return action();
    });
    b.setAttribute("role", "menuitem");
    b.append(el("span", "", label), el("kbd", "", shortcut));
    menu.append(b);
    return b;
  };
  const copyButton = item(tr("Kopyala"), "Ctrl+Shift+C", copy);
  const pasteButton = item(tr("Yapıştır"), "Ctrl+V", paste);
  const imageButton = sessionId && item(tr("Dosya ekle…"), "", async () => {
    const run = writableRun();
    if (!run) return;
    const images = await chooseSessionImages(sessionId);
    if (!images.length || writableRun() !== run || !host.isConnected) return;
    const selected = session(sessionId);
    if (!selected || selected.login) return;
    await paste(() => " " + imagePrompt(images, selected.provider) + " ");
    term.focus();
  });
  host.append(menu);
  host.addEventListener("contextmenu", (e) => {
    // Preserve mouse reporting in full-screen CLI applications (Shift overrides it).
    if (term.modes.mouseTrackingMode !== "none" && !e.shiftKey) return;
    e.preventDefault();
    e.stopPropagation();
    copyButton.disabled = !term.hasSelection();
    pasteButton.disabled = !writableRun() || term.options.disableStdin;
    if (imageButton) imageButton.disabled = pasteButton.disabled;
    menu.showPopover();
    const rect = host.getBoundingClientRect();
    const x = e.clientX || rect.left + 12, y = e.clientY || rect.top + 12;
    menu.style.left = Math.max(4, Math.min(x, innerWidth - menu.offsetWidth - 4)) + "px";
    menu.style.top = Math.max(4, Math.min(y, innerHeight - menu.offsetHeight - 4)) + "px";
    (copyButton.disabled ? pasteButton : copyButton).focus();
  }, true);
  menu.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      menu.hidePopover();
      term.focus();
    } else if (["ArrowDown", "ArrowUp"].includes(e.key)) {
      e.preventDefault();
      const next = document.activeElement === copyButton ? pasteButton : copyButton;
      if (!next.disabled) next.focus();
    }
  });
  return {
    hasPendingInput: () => pendingInput > 0,
    attachFiles: (paths) => paste(async () => {
      const files = await attachmentReferences(sessionId, paths);
      return " " + imagePrompt(files, session(sessionId)?.provider) + " ";
    }),
  };
}

function confirmTerminalPaste() {
  if ($("#terminal-paste-confirmation")) return Promise.resolve(false);
  const dialog = el("dialog");
  dialog.id = "terminal-paste-confirmation";
  dialog.setAttribute("aria-labelledby", "terminal-paste-title");
  const header = el("header", "dialog-header");
  const title = el("h2", "", tr("Terminale yapıştır"));
  title.id = "terminal-paste-title";
  header.append(title);
  const body = el("div", "dialog-body");
  body.append(el("p", "help", tr("Bu terminalde satır sonları komutu çalıştırabilir. Satır sonları ve sekmeler boşluğa çevrilsin mi?")));
  const footer = el("footer", "dialog-footer");
  const cancel = button(tr("Vazgeç"), () => dialog.close());
  cancel.autofocus = true;
  footer.append(cancel, button(tr("Tek satır olarak yapıştır"), () => dialog.close("paste"), "primary"));
  dialog.append(header, body, footer);
  document.body.append(dialog);
  hideBrowser();
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => {
      dialog.remove();
      scheduleViewport();
      resolve(dialog.returnValue === "paste");
    }, { once: true });
    dialog.showModal();
  });
}

function createCard(s, index) {
  const card = el("article", "session-card");
  card.dataset.id = s.id;
  const head = el("header", "card-header"),
    title = el("strong", "", s.name),
    model = button(s.model || tr("Model seç⌄"), () => openModels(s.id)),
    control = button("▶", () => toggleTerminal(s.id), "icon"),
    preview = button(tr("Önizleme"), () => usePreview(s.id), "preview-button"),
    link = button(
      "⛓",
      () => {},
      "link-handle",
      tr("Başka oturuma bağlamak için sürükle"),
    ),
    agentBadge = el("span", "badge", countLabel(0, "{count} ajan")),
    state = stateBadge(s.id);
  head.append(
    el("span", "provider-icon " + s.provider, icons[s.provider]),
    title,
    button("✎", () => rename(s.id), "icon quiet", tr("Adı değiştir")),
    agentBadge,
    model,
    preview,
    workspaceMenuButton(tr("Oturum seçenekleri"), () => sessionMenu(s.id), "icon"),
    link,
    state,
    control,
    button("×", () => archiveSession(s.id), "icon quiet", tr("Arşive taşı")),
  );
  card.append(head);
  const notice = el("div", "card-notice");
  notice.hidden = true;
  card.append(notice);
  const host = el("div", "terminal-host"),
    overlay = el("div", "terminal-overlay"),
    overlayText = el("p", "", tr("Oturum hazırlanıyor…"));
  overlay.append(overlayText);
  host.append(overlay);
  const foot = el("div", "card-footer"),
    path = el("span", "card-path"),
    pathMode = el("span", "card-path-mode"),
    pathTail = el("span", "card-path-value"),
    pathValue = el("bdi"),
    account = el("span", "card-account"),
    summary = button(
      tr("Özet"),
      () => selectSession(s.id, false, "summary"),
      "card-summary",
    );
  pathValue.dir = "ltr";
  pathTail.append(pathValue);
  path.append(pathMode, pathTail);
  account.hidden = true;
  foot.append(path, account, summary);
  card.append(host, foot);
  const term = new Terminal({
      fontFamily: 'Consolas, "Cascadia Code", monospace',
      fontSize: 13,
      lineHeight: 1.15,
      cursorBlink: true,
      scrollback: 20000,
      allowProposedApi: false,
      theme: {
        background: "#0c1015",
        foreground: "#dbe4ee",
        cursor: "#beec91",
        selectionBackground: "#41576c",
      },
    }),
    fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  const holder = el("div");
  holder.style.cssText = "position:absolute;inset:9px 3px 3px 10px";
  host.prepend(holder);
  term.open(holder);
  const v = {
    card,
    head,
    title,
    model,
    control,
    preview,
    link,
    state,
    path,
    pathMode,
    pathValue,
    account,
    summary,
    notice,
    host,
    overlay,
    overlayText,
    term,
    fit,
    runId: null,
    seq: 0,
    ready: false,
    replaying: true,
    queue: [],
    hasOutput: false,
    inputDraft: "",
    busy: false,
    activityText: "",
    activityTimer: null,
    agentBadge,
    childAgents: new Set(),
    observedModel: null,
  };
  term.onWriteParsed(() => observeTerminal(s.id, v));
  const sendInput = (data) => {
    if (
      v.replaying ||
      !active(s.id) ||
      !v.runId ||
      /^\x1b\[[?0-9;]*[Rc]$/.test(data)
    )
      return;
    if (data === "\r") v.inputDraft = "";
    else if (data === "\x7f") v.inputDraft = v.inputDraft.slice(0, -1);
    else if (data.startsWith("\x1b[200~") && data.endsWith("\x1b[201~"))
      v.inputDraft += data.slice(6, -6);
    else if (!/^[\x00-\x1f\x7f]/.test(data)) v.inputDraft += data;
    return api("terminal", "write", { id: s.id, runId: v.runId, data });
  };
  const clipboard = terminalClipboard(term, host, () =>
    views.get(s.id) === v && v.ready && !v.replaying && active(s.id) ? v.runId : null,
    sendInput, s.login ? null : s.id,
  );
  v.hasPendingInput = clipboard.hasPendingInput;
  v.attachFiles = clipboard.attachFiles;
  const observer = new ResizeObserver(() => {
    clearTimeout(v.resizeTimer);
    v.resizeTimer = setTimeout(() => {
      fitOne(v);
      drawEdges();
    }, 70);
  });
  v.observer = observer;
  observer.observe(holder);
  card.addEventListener("pointerdown", (e) => {
    if (e.target.closest(".card-summary")) return;
    if (focusId !== s.id) {
      focusId = s.id;
      renderTeamsEditor();
      renderSessionList();
      renderSessionTabs();
      renderChatView();
      $$(".session-card").forEach((c) =>
        c.classList.toggle("focused", c.dataset.id === s.id),
      );
      drawEdges();
      refreshPanel();
      remember();
    }
  });
  head.addEventListener("pointerdown", (e) => {
    if (e.target.closest("button") || mode !== "map" || e.button !== 0) return;
    e.preventDefault();
    head.setPointerCapture(e.pointerId);
    gesture = {
      kind: "card",
      id: s.id,
      x: e.clientX,
      y: e.clientY,
      left: parseFloat(card.style.left) || 0,
      top: parseFloat(card.style.top) || 0,
    };
  });
  link.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.stopPropagation();
    e.preventDefault();
    connecting = { source: s.id, x: e.clientX, y: e.clientY };
    link.setPointerCapture(e.pointerId);
  });
  card.addEventListener("pointerup", () => {
    if (!gesture) {
      const width = card.offsetWidth,
        height = card.offsetHeight;
      if (
        mode === "map" &&
        (width !== (session(s.id).size?.width || 720) ||
          height !== (session(s.id).size?.height || 500))
      )
        saveCard(s.id, { size: { width, height } });
    }
  });
  title.ondblclick = () => selectSession(s.id, true);
  return v;
}
async function attach(v, s) {
  v.ready = false;
  v.replaying = true;
  const snapshot = await api("terminal", "attach", { id: s.id });
  v.runId = snapshot.runId || null;
  v.seq = snapshot.seq || 0;
  v.term.reset();
  const bytes = decode(snapshot.data || "");
  v.hasOutput = bytes.length > 0;
  await new Promise((resolve) => v.term.write(bytes, resolve));
  v.ready = true;
  v.replaying = false;
  for (const p of v.queue.splice(0)) terminalPacket(p);
  v.overlay.hidden = v.hasOutput || active(s.id);
  fitOne(v);
}
function decode(b64) {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}
function terminalPacket(p) {
  const v = views.get(p.id);
  if (!v) return;
  if (!v.ready) {
    v.queue.push(p);
    return;
  }
  if (p.runId !== v.runId) return;
  if (p.exitCode !== undefined) {
    activity.delete(p.id);
    sync().catch(error);
    return;
  }
  if (p.seq <= v.seq) return;
  v.seq = p.seq;
  v.hasOutput = true;
  v.overlay.hidden = true;
  v.term.write(decode(p.data));
  const text = new TextDecoder()
    .decode(decode(p.data))
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "");
  v.activityText = (v.activityText + text).slice(-6000);
  detectActivity(p.id, v);
}
function detectActivity(id, v) {
  clearTimeout(v.activityTimer);
  const text = v.activityText.slice(-2200);
  let s;
  if (
    /allow.*(session|tool)|permission|approve|izin|Do you want to proceed/i.test(
      text.slice(-1200),
    )
  )
    s = { key: "permission", label: tr("İzin bekliyor") };
  else if (
    /Working \(|Thinking|esc to interrupt|ctrl.c to interrupt|Running…/i.test(
      text.slice(-1200),
    )
  )
    s = { key: "working", label: tr("Çalışıyor") };
  else if (/Worked for|Ready >|Ask Codex|How can I help|❯/i.test(text))
    s = { key: "completed", label: tr("Tamamlandı") };
  else s = { key: "running", label: tr("Açık") };
  const old = activity.get(id);
  activity.set(id, s);
  if (s.key !== old?.key) {
    if (session(id)?.projectId === projectId) {
      renderSessionList();
      renderSessionTabs();
    }
    const view = views.get(id);
    if (view) view.state.replaceWith((view.state = stateBadge(id)));
  }
  v.activityTimer = setTimeout(() => {
    if (active(id) && activity.get(id)?.key === "working" && !v.inputDraft) {
    }
  }, 3000);
}
function observeTerminal(id, v) {
  const b = v.term.buffer.active;
  const rows = [];
  for (let i = Math.max(0, b.length - 10); i < b.length; i++)
    rows.push(b.getLine(i)?.translateToString(true) || "");
  const footer = rows
    .reverse()
    .find((l) =>
      /^\s*(?:gpt[- .]|claude[- ]|sonnet\b|opus\b|haiku\b|model:)/i.test(l),
    );
  if (footer) {
    const m = footer.match(
      /(?:gpt[-\w.]+|claude[-\w.]+|(?:Sonnet|Opus|Haiku)\s*[\d.]*)/i,
    );
    if (m) {
      v.observedModel = m[0];
      if (session(id)?.provider === "codex") {
        v.observedEffort = footer.slice(m.index + m[0].length)
          .match(/^\s+(none|minimal|low|medium|high|xhigh|max|ultra)\b/i)?.[1]?.toLowerCase();
      }
      v.model.textContent = m[0];
    }
  }
  const text = v.activityText;
  for (const m of text.matchAll(
    /(Started|Completed|Closed)\s+[`']?([^\s`']+\/root\/[^\s`']+|\/root\/[^\s`']+)/g,
  )) {
    if (m[1] === "Started") v.childAgents.add(m[2]);
    else v.childAgents.delete(m[2]);
  }
  const agentCount = active(id) && !session(id)?.login ? 1 + v.childAgents.size : 0;
  v.agentBadge.textContent = countLabel(agentCount, "{count} ajan");
}
function fitOne(v) {
  if (v.card.hidden) return;
  try {
    v.fit.fit();
    if (active(v.card.dataset.id) && v.runId)
      api("terminal", "resize", {
        id: v.card.dataset.id,
        runId: v.runId,
        cols: v.term.cols,
        rows: v.term.rows,
      }).catch(() => {});
  } catch {}
}
function fitTerminals() {
  for (const v of views.values()) fitOne(v);
}
async function conversationFromLogin(id) {
  await api("app", "conversation_from_login", { id });
  await sync();
  await toggleTerminal(id);
}
function confirmSharingWarnings({ files, truncated }) {
  if (!files.length && !truncated) return Promise.resolve(true);
  const dialog = el("dialog");
  const title = el("h2", "", tr(files.length ? "Hassas olabilecek proje dosyaları" : "Dosya taraması eksik kaldı"));
  title.id = "sharing-warning-title";
  dialog.setAttribute("aria-labelledby", title.id);
  const header = el("header", "dialog-header");
  header.append(title);
  const body = el("div", "dialog-body");
  if (files.length) {
    body.append(el("p", "help", tr("Bu dosyalar kimlik bilgileri içerebilir. Sağlayıcı kendi araçlarıyla çalışma alanındaki dosyalara erişebilir.")));
    const list = el("ul");
    for (const file of files) list.append(el("li", "", file));
    body.append(list);
  } else {
    body.append(el("p", "help", tr("Taranan kısımda hassas olabilecek dosya bulunmadı.")));
  }
  if (truncated) body.append(el("p", "help", tr("Bazı dosyalar tam olarak taranamadı.")));
  const footer = el("footer", "dialog-footer");
  const cancel = button(tr("Vazgeç"), () => dialog.close());
  cancel.autofocus = true;
  footer.append(cancel, button(tr("Devam et"), () => dialog.close("continue"), "primary"));
  dialog.append(header, body, footer);
  document.body.append(dialog);
  hideBrowser();
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => {
      dialog.remove();
      scheduleViewport();
      resolve(dialog.returnValue === "continue");
    }, { once: true });
    dialog.showModal();
  });
}
async function toggleTerminal(id) {
  if (starting.has(id)) return;
  if (active(id)) {
    await api("terminal", "stop", { id, runId: views.get(id)?.runId });
    toast(tr("Oturum durduruluyor"));
    return;
  }
  starting.add(id);
  renderCards();
  try {
    if (!session(id)?.login) {
      const warnings = await api("app", "sharing_warnings", { sessionId: id });
      if (!await confirmSharingWarnings(warnings)) return;
    }
    const snapshot = await api("terminal", "start", { id });
    await sync();
    const v = views.get(id);
    if (v) await attach(v, session(id));
    return snapshot;
  } finally {
    starting.delete(id);
    await sync();
  }
}
function hasUnmovedArchive(s) {
  return s.archived && !s.archivedFromDirectory && ["worktree", "copy"].includes(s.workspaceKind);
}

async function archiveProcesses(selected, state) {
  const id = selected.id;
  if (starting.has(id)) throw Error(tr("Oturum açılıyor. Biraz sonra tekrar dene."));
  const preview = selected.preview || previews[id] ? await api("preview", "status", { sessionId: id }) : null;
  return {
    terminal: state.active.find((run) => run.id === id),
    preview: ["starting", "ready"].includes(preview?.state) ? preview : null,
  };
}

async function stopArchiveProcesses(id, terminal, preview) {
  if (preview) await api("preview", "stop", { sessionId: id, runId: preview.runId });
  if (!terminal) return;
  await api("terminal", "stop", { id, runId: terminal.runId });
  const deadline = Date.now() + 5000;
  while ((await api("app", "state")).active.some((run) => run.runId === terminal.runId)) {
    if (Date.now() >= deadline) throw Error(tr("Terminal henüz durmadı. Durduktan sonra tekrar dene."));
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

async function archiveSession(id, fromHistory = false) {
  const selected = session(id);
  if (!selected || (selected.archived && !hasUnmovedArchive(selected))) return;
  const dialog = $("#dialog");
  const origin = dialog.open && $("#dialog-body").firstElementChild;
  const { terminal, preview } = await archiveProcesses(selected, await api("app", "state"));
  const save = async () => {
    await api("app", "session_patch", { id, archived: true });
    await sync();
    refreshPanel().catch(error);
    toast(tr("Oturum arşive taşındı."));
  };
  if (!terminal && !preview) {
    await save();
    if (origin && dialog.open && $("#dialog-body").firstElementChild === origin) {
      if (fromHistory) archiveList();
      else closeModal();
    }
    return;
  }
  modal(
    tr("Oturumu arşivle") + " · " + selected.name,
    el("p", "help", tr("Bu oturumun çalışan terminali ve varsa açık önizlemesi durdurulacak. Konuşma ve çalışma alanı arşivde korunur.")),
    async () => {
      await stopArchiveProcesses(id, terminal, preview);
      await save();
    },
    tr("Durdur ve arşivle"),
  );
}
function sessionMenu(id) {
  const s = session(id);
  if (!s || s.archived) return [];
  const bind = button(tr("Konuşma seç / doğrula"), () => bindHistory(id));
  if (active(id) && s.conversationId) {
    bind.disabled = true;
    bind.title = tr("Konuşmayı değiştirmek için önce oturumu durdur");
  }
  return [
    button(tr("Konuşma geçmişini oku"), () => showTranscript(id)),
    bind,
    ...(foldersFor(s.projectId).length ? [button(tr("Klasöre taşı"), () => moveSessionDialog(s))] : []),
    ...(s.workspaceKind === "direct"
      ? []
      : [button(tr("Eklentiler"), () => sessionAddons(id))]),
    ...(s.login ? [] : [button(tr("Önizleme ayarları"), () => previewSettings(s))]),
    ...(s.initialPrompt ? [button(tr("Şablon istemini göster"), () => {
      const prompt = el("pre", "template-prompt", s.initialPrompt);
      const copy = button(tr("Kopyala"), () => api("clipboard", "write", { text: s.initialPrompt }));
      modal(tr("Şablon istemi"), [prompt, copy]);
    })] : []),
    ...(canSetTeamsDestination(s)
      ? [button(tr("Teams hedefi"), () => teamsDestination(s))]
      : []),
    button(tr("Çalışma klasörünü aç"), () =>
      api("app", "reveal", { sessionId: id }),
    ),
    button(tr("Oturum bilgileri"), () => showSessionInfo(s)),
    button(tr("Arşive taşı"), () => archiveSession(id)),
  ];
}
function showSessionInfo(s) {
  const details = el(
    "p",
    "help",
    s.directory +
      tr("\nKonuşma: ") +
      (s.conversationId ||
        (s.workspaceKind === "direct" && s.provider === "codex"
          ? tr("Konuşma seç / doğrula ile bağla")
          : tr("İlk mesajdan sonra kaydedilir"))),
  );
  modal(tr("Oturum bilgileri") + " · " + s.name, details);
}
async function showTranscript(id, fromHistory = false) {
  const result = await api("history", "messages", { id });
  const box = el("div");
  if (fromHistory) box.append(button(tr("Oturum geçmişine dön"), archiveList, "history-back"));
  if (session(id)?.archivedFromDirectory) {
    box.append(button(tr("Arşiv klasörünü aç"), () => api("app", "reveal", { sessionId: id }), "history-back"));
  }
  for (const m of result.messages) {
    const row = el("article", "transcript-message " + m.role);
    row.append(
      el("strong", "", m.role === "user" ? tr("Sen") : names[result.provider]),
      el("pre", "", m.text),
    );
    box.append(row);
  }
  if (!result.messages.length) {
    box.append(
      el(
        "p",
        "help",
        tr("Yapılandırılmış mesaj kaydı henüz bulunamadı. Kartta kaydedilmiş terminal çıktısı görüntülenir."),
      ),
    );
  }
  modal(session(id).name + tr(" · Geçmiş"), box);
}
async function bindHistory(id) {
  const found = await api("history", "list", { id });
  const choice = select(
    found.map((t) => ({
      id: t.id,
      label: t.id.slice(0, 8) + " · " + (t.createdAt || ""),
    })),
    session(id).conversationId,
  );
  modal(
    tr("Konuşmayı seç"),
    [
      el(
        "p",
        "help",
        session(id).workspaceKind === "direct"
          ? tr("Bu klasördeki sağlayıcı konuşmaları listelenir. Sürdürmek istediğin konuşmayı seç.")
          : tr("Yalnız bu oturumun çalışma alanına ait kayıtlar listelenir. Bağlamak terminale giriş göndermez."),
      ),
      choice,
    ],
    async () => {
      await api("history", "bind", { id, conversationId: choice.value });
      await sync();
    },
  );
}
function modelChangeBlock(id) {
  if (!active(id)) return "";
  const view = views.get(id);
  if (!view?.ready || !view.runId) return tr("Terminalin hazırlanmasını bekle.");
  const state = status(id).key;
  let codexPromptReady = false;
  if (session(id)?.provider === "codex") {
    const buffer = view.term.buffer.active;
    const lines = Array.from(
      { length: view.term.rows },
      (_, row) =>
        buffer.getLine(buffer.baseY + row)?.translateToString(true) || "",
    );
    const screen = lines.join("\n");
    if (
      /Update available/i.test(screen) &&
      /Update now|Skip until next version|esc skip|esc to skip/i.test(screen)
    )
      return tr("Codex güncelleme ekranında bekliyor. Terminalde Esc ile geç veya güncellemeyi tamamla, ardından modeli uygula.");
    if (
      /Do you trust|Do you want to proceed|Would you like to run the following|Allow[\s\S]{0,150}to run tool|Allow for this session|Approve for this session/i.test(
        screen,
      )
    )
      return tr("Codex terminalde bir onay bekliyor. Kararını terminalde verip model seçimine dön.");
    codexPromptReady = lines
      .filter((line) => line.trim())
      .slice(-8)
      .some((line) => /^\s*[›❯>]?\s*Ask Codex to do anything\s*$/i.test(line));
  }
  if (view.hasPendingInput() || view.inputDraft.trim())
    return tr("Terminalde bir taslak var. Modeli uygulamadan önce taslağı temizle veya gönder.");
  if (state === "working")
    return tr("Ajan çalışıyor. Yanıtı bitirip girişe döndüğünde modeli uygula.");
  if (state === "permission" && !codexPromptReady)
    return tr("Ajan terminalde bir onay bekliyor. Kararını terminalde verip model seçimine dön.");
  return "";
}

async function openModels(id) {
  const b = views.get(id)?.model;
  if (b) {
    b.disabled = true;
    b.textContent = tr("Yükleniyor…");
  }
  try {
    const models = await api("models", "list", { id });
    const s = session(id),
      choices = select(
        models.map((m) => ({ id: m.id, label: m.name || m.id })),
        (
          models.find((m) => m.id === s.model) ||
          models.find((m) => m.default) ||
          models[0]
        )?.id,
      ),
      effort = select([], s.effort);
    function update() {
      effort.replaceChildren();
      const m = models.find((m) => m.id === choices.value);
      for (const e of m?.efforts || [])
        effort.add(new Option(e.reasoningEffort, e.reasoningEffort));
      effort.hidden = !effort.options.length;
      const saved = [...effort.options].some(
        (option) => option.value === s.effort,
      );
      if (saved) effort.value = s.effort;
      else if (
        [...effort.options].some((option) => option.value === m?.defaultEffort)
      )
        effort.value = m.defaultEffort;
    }
    choices.onchange = update;
    update();
    modal(
      tr("Model seç"),
      [
        field("Model", choices),
        field(tr("Düşünme seviyesi"), effort),
        el(
          "p",
          "help",
          modelChangeBlock(id) ||
            (active(id)
              ? tr("Ajan giriş beklerken uygula. Terminalde yazılmış bir taslak varsa önce temizle.")
              : tr("Seçimin bu oturumun sonraki başlangıcında kullanılır.")),
        ),
      ],
      async () => {
        const v = views.get(id);
        const blocked = modelChangeBlock(id);
        if (blocked) throw Error(blocked);
        await api("models", "apply", {
          id,
          model: choices.value,
          effort: effort.hidden ? null : effort.value,
          runId: v?.runId,
          seq: v?.seq,
        });
        await sync();
      },
      tr("Uygula"),
    );
  } finally {
    if (b) {
      b.disabled = false;
      b.textContent =
        views.get(id)?.observedModel || session(id)?.model || tr("Model seç⌄");
    }
  }
}

function applyCamera() {
  const world = $("#world");
  world.style.transform =
    `translate(${camera.x}px,${camera.y}px) scale(${camera.z})`;
  world.style.setProperty("--edge-node-scale", Math.max(1, 0.6 / camera.z));
  linkTooltip.hidden = true;
  $("#zoom").textContent = Math.round(camera.z * 100) + "%";
  scheduleNotificationContext();
}
function saveCard(id, patch) {
  Object.assign(session(id), patch);
  drawEdges();
  return api("app", "session_patch", { id, ...patch }).catch(error);
}
function setMode(value) {
  mode = value;
  document.body.classList.toggle("grid", mode !== "map");
  document.body.classList.toggle("focus", mode === "focus");
  for (const n of ["map", "grid", "focus"])
    $("#view-" + n).classList.toggle("selected", n === mode);
  applyCamera();
  drawEdges();
  fitTerminals();
  scheduleNotificationContext();
  remember();
}
function zoom(amount, x, y) {
  if (mode === "focus") setMode("map");
  const rect = $("#viewport").getBoundingClientRect();
  x ??= rect.width / 2;
  y ??= rect.height / 2;
  const z = Math.max(0.2, Math.min(1.5, camera.z * amount)),
    ratio = z / camera.z;
  camera.x = x - (x - camera.x) * ratio;
  camera.y = y - (y - camera.y) * ratio;
  camera.z = z;
  applyCamera();
  remember();
}
function fitMap() {
  const cards = visibleSessions()
    .map((s) => views.get(s.id)?.card)
    .filter(Boolean);
  if (!cards.length) return;
  const left = Math.min(...cards.map((c) => parseFloat(c.style.left))),
    top = Math.min(...cards.map((c) => parseFloat(c.style.top))),
    right = Math.max(
      ...cards.map((c) => parseFloat(c.style.left) + c.offsetWidth),
    ),
    bottom = Math.max(
      ...cards.map((c) => parseFloat(c.style.top) + c.offsetHeight),
    );
  const r = $("#viewport").getBoundingClientRect();
  camera.z = Math.max(
    0.2,
    Math.min(
      1,
      (r.width - 70) / (right - left),
      (r.height - 80) / (bottom - top),
    ),
  );
  camera.x = (r.width - (right - left) * camera.z) / 2 - left * camera.z;
  camera.y = (r.height - (bottom - top) * camera.z) / 2 - top * camera.z;
  applyCamera();
  remember();
}
function focusCard() {
  if (views.has(focusId)) setMode("focus");
}
function centerCard() {
  const c = views.get(focusId)?.card;
  if (!c) return;
  const r = $("#viewport").getBoundingClientRect();
  camera.z = Math.min(
    1,
    (r.width - 40) / c.offsetWidth,
    (r.height - 40) / c.offsetHeight,
  );
  camera.x =
    (r.width - c.offsetWidth * camera.z) / 2 -
    parseFloat(c.style.left) * camera.z;
  camera.y =
    (r.height - c.offsetHeight * camera.z) / 2 -
    parseFloat(c.style.top) * camera.z;
  applyCamera();
  remember();
}
function linkPermissionNames(link) {
  const names = [];
  if (link.read) names.push(tr("Konuşma"));
  if (link.files) names.push(tr("Dosya"));
  if (link.work) names.push(tr("İş"));
  return names.length ? names : [tr("İzin yok")];
}

const linkTooltip = el("div", "link-tooltip");
linkTooltip.hidden = true;
linkTooltip.setAttribute("aria-hidden", "true");
document.body.append(linkTooltip);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") linkTooltip.hidden = true;
});

function showLinkTooltip(node, direction, permissions, needsReapproval) {
  const rect = node.getBoundingClientRect();
  linkTooltip.replaceChildren(
    el("strong", "", direction),
    el("span", "", permissions.join(" · ")),
  );
  if (needsReapproval)
    linkTooltip.append(el("span", "link-reapproval", tr("Yeniden onay gerekli")));
  linkTooltip.hidden = false;
  const left = rect.left + (rect.width - linkTooltip.offsetWidth) / 2;
  const below = rect.bottom + 6;
  const top = below + linkTooltip.offsetHeight <= innerHeight - 8
    ? below : rect.top - linkTooltip.offsetHeight - 6;
  linkTooltip.style.left = Math.max(8, Math.min(left, innerWidth - linkTooltip.offsetWidth - 8)) + "px";
  linkTooltip.style.top = Math.max(8, top) + "px";
}

function edgeRoute(a, b, reciprocal) {
  const ax = a.offsetLeft, ay = a.offsetTop, bx = b.offsetLeft, by = b.offsetTop;
  const aw = a.offsetWidth, ah = a.offsetHeight, bw = b.offsetWidth, bh = b.offsetHeight;
  const dx = bx + bw / 2 - (ax + aw / 2), dy = by + bh / 2 - (ay + ah / 2);
  const horizontalGap = Math.max(bx - (ax + aw), ax - (bx + bw));
  const verticalGap = Math.max(by - (ay + ah), ay - (by + bh));
  const horizontal = horizontalGap >= 0 || (verticalGap < 0 && Math.abs(dx) >= Math.abs(dy));
  const direction = (horizontal ? dx : dy) >= 0 ? 1 : -1;
  // Keep reciprocal links and their nodes separately clickable.
  const lane = reciprocal ? direction * (horizontal ? 46 : 58) : 0;
  const x1 = horizontal ? ax + (direction > 0 ? aw : 0) : ax + aw / 2 + lane;
  const x2 = horizontal ? bx + (direction < 0 ? bw : 0) : bx + bw / 2 + lane;
  const spread = horizontal && horizontalGap < 100 ? 36 : 0;
  const y1 = horizontal ? ay + 132 + lane - spread : ay + (direction > 0 ? ah : 0);
  const y2 = horizontal ? by + 132 + lane + spread : by + (direction < 0 ? bh : 0);
  const bend = direction * Math.max(12, Math.abs(horizontal ? x2 - x1 : y2 - y1) / 2);
  const controlX = horizontal ? bend : 0, controlY = horizontal ? 0 : bend;
  return {
    d: `M${x1} ${y1}C${x1 + controlX} ${y1 + controlY},${x2 - controlX} ${y2 - controlY},${x2} ${y2}`,
    x: (x1 + x2) / 2,
    y: (y1 + y2) / 2,
  };
}

function drawEdges() {
  const group = $("#edge-lines");
  const labels = $("#edge-labels");
  const focusedNode = document.activeElement?.dataset.linkDirection;
  linkTooltip.hidden = true;
  group.replaceChildren();
  labels.replaceChildren();
  for (const { card } of views.values()) card.classList.remove("linked-focus");
  if (mode !== "map") return;
  const highlight = panel === "summary" && focusId;
  const directions = new Set(db.links.map(({ source, target }) => source + ":" + target));
  for (const link of db.links) {
    const a = views.get(link.source)?.card,
      b = views.get(link.target)?.card;
    if (!a || !b || a.hidden || b.hidden) continue;
    const route = edgeRoute(a, b, directions.has(link.target + ":" + link.source));
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", route.d);
    const focused =
      highlight && (link.source === focusId || link.target === focusId);
    const emphasis = focused ? " edge-focused" : highlight ? " edge-muted" : "";
    const reapproval = link.needsReapproval ? " edge-reapproval" : "";
    if (focused) (link.source === focusId ? b : a).classList.add("linked-focus");
    const direction = link.source + ":" + link.target;
    const edit = () => openLink(link.source, link.target);
    path.setAttribute("class", "edge" + emphasis + reapproval);
    path.setAttribute("marker-end", link.needsReapproval ? "url(#arrow-reapproval)" : "url(#arrow)");
    path.onclick = () => edit().catch(error);
    const permissions = linkPermissionNames(link);
    const description = session(link.source).name + " → " + session(link.target).name;
    const accessibleLabel = [description, ...permissions];
    if (link.needsReapproval) accessibleLabel.push(tr("Yeniden onay gerekli"));
    const node = button("", async () => {
      linkTooltip.hidden = true;
      await edit();
      $("#dialog").addEventListener("close", () => {
        [...labels.children].find((b) => b.dataset.linkDirection === direction)
          ?.focus({ preventScroll: true });
      }, { once: true });
    }, "link-node" + emphasis + reapproval);
    node.setAttribute("aria-label", accessibleLabel.join(" · "));
    node.dataset.linkId = link.id;
    node.dataset.linkDirection = direction;
    node.style.left = route.x + "px";
    node.style.top = route.y + "px";
    const icon = el("span", link.needsReapproval ? "link-warning-mark" : "", link.needsReapproval ? "!" : "⛓\uFE0E");
    icon.setAttribute("aria-hidden", "true");
    node.append(icon);
    const showPermissions = () => showLinkTooltip(node, description, permissions, link.needsReapproval);
    node.onpointerenter = showPermissions;
    node.onpointerleave = () => {
      if (!node.matches(":focus-visible")) linkTooltip.hidden = true;
    };
    node.onfocus = () => {
      if (node.matches(":focus-visible")) showPermissions();
    };
    node.onblur = () => {
      if (!node.matches(":hover")) linkTooltip.hidden = true;
    };
    group.append(path);
    labels.append(node);
    if (direction === focusedNode && !$("#dialog").open) node.focus({ preventScroll: true });
  }
}
$("#viewport").addEventListener("pointerdown", (e) => {
  if (
    shownChatId || mode !== "map" ||
    e.button !== 0 ||
    e.target.closest(".session-card") ||
    e.target.closest(".edge, .link-node")
  )
    return;
  e.preventDefault();
  $("#viewport").setPointerCapture(e.pointerId);
  gesture = { kind: "pan", x: e.clientX, y: e.clientY, camera: { ...camera } };
});
function drawConnectionPreview() {
  document.getElementById("connection-preview")?.remove();
  if (!connecting) return;
  const origin = views.get(connecting.source)?.link.getBoundingClientRect(),
    r = $("#viewport").getBoundingClientRect();
  if (!origin) return;
  const x1 = (origin.x + origin.width / 2 - r.x - camera.x) / camera.z,
    y1 = (origin.y + origin.height / 2 - r.y - camera.y) / camera.z,
    x2 = (connecting.x - r.x - camera.x) / camera.z,
    y2 = (connecting.y - r.y - camera.y) / camera.z;
  const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
  p.id = "connection-preview";
  p.setAttribute("class", "edge");
  p.setAttribute("stroke-dasharray", "6 5");
  p.style.pointerEvents = "none";
  p.setAttribute(
    "d",
    `M${x1} ${y1} C${x1 + 80} ${y1},${x2 - 80} ${y2},${x2} ${y2}`,
  );
  $("#edge-lines").append(p);
}
window.addEventListener("pointermove", (e) => {
  if (connecting) {
    connecting.x = e.clientX;
    connecting.y = e.clientY;
    drawConnectionPreview();
    return;
  }
  if (!gesture) return;
  if (gesture.kind === "pan") {
    camera.x = gesture.camera.x + e.clientX - gesture.x;
    camera.y = gesture.camera.y + e.clientY - gesture.y;
    applyCamera();
  } else {
    const c = views.get(gesture.id).card;
    c.style.left = gesture.left + (e.clientX - gesture.x) / camera.z + "px";
    c.style.top = gesture.top + (e.clientY - gesture.y) / camera.z + "px";
    drawEdges();
  }
});
window.addEventListener("pointerup", (e) => {
  if (connecting) {
    const source = connecting.source;
    connecting = null;
    drawConnectionPreview();
    const target = document.elementsFromPoint(e.clientX, e.clientY)
      .map((element) => element.closest(".session-card"))
      .find(Boolean)?.dataset.id;
    if (target && target !== source) openLink(source, target).catch(error);
    return;
  }
  if (gesture) {
    if (gesture.kind === "card") {
      const c = views.get(gesture.id).card;
      saveCard(gesture.id, {
        position: { x: parseFloat(c.style.left), y: parseFloat(c.style.top) },
      });
    }
    gesture = null;
    remember();
  }
});
$("#viewport").addEventListener(
  "wheel",
  (e) => {
    if (shownChatId || mode !== "map" || !e.ctrlKey) return;
    e.preventDefault();
    const r = $("#viewport").getBoundingClientRect();
    zoom(Math.exp(-e.deltaY * 0.002), e.clientX - r.left, e.clientY - r.top);
  },
  { passive: false },
);
$("#zoom-out").onclick = () => zoom(1 / 1.15);
$("#zoom-in").onclick = () => zoom(1.15);
$("#fit").onclick = () => {
  if (mode === "focus") setMode("map");
  fitMap();
};
$("#focus-card").onclick = focusCard;
for (const m of ["map", "grid", "focus"])
  $("#view-" + m).onclick = () => setMode(m);
function remember() {
  if (projectId) {
    if (focusId) projectFocus[projectId] = focusId;
    projectFolders[projectId] = folderId;
    folderLayout[folderLayoutKey()] = { focusId, camera };
  }
  localStorage.setItem(
    "agentloom-layout",
    JSON.stringify({
      projectId,
      focusId,
      mode,
      camera,
      projectFocus,
      folderId,
      projectFolders,
      folderLayout,
      collapsedSessionFolders: [...collapsedSessionFolders],
      panel,
      changesPreference,
      noLeft: document.body.classList.contains("no-left"),
      noRight: document.body.classList.contains("no-right"),
      showProviders: !$("#providers-section").hidden,
    }),
  );
}
$("#app-settings").onclick = () => {
  const language = select([
    { id: "tr", label: "Türkçe" },
    { id: "en", label: "English" },
  ], uiLanguage);
  language.id = "language-select";
  language.onchange = () => {
    localStorage.setItem("workflow-language", language.value);
    remember();
    location.reload();
  };
  modal(tr("Ayarlar"), [
    field(tr("Arayüz dili"), language),
    notificationSettings(),
    button(tr("Sağlayıcı kurulumu"), () => openProviderSetup()),
    button(tr("Hakkında ve güncellemeler"), openAbout),
  ]);
  $("#dialog-footer button").textContent = tr("Kapat");
};

async function openAbout() {
  const panel = el("div", "about-updates");
  panel.id = "about-updates";
  panel.textContent = tr("Yükleniyor…");
  modal(tr("Hakkında ve güncellemeler"), panel);
  $("#dialog-footer button").textContent = tr("Kapat");
  paintUpdate(await api("updates", "status"), panel);
}

const updateNotice = $("#update-notice");
let dismissedUpdateVersion;

async function restartForUpdate() {
  if (dialogCloseGuard && !dialogCloseGuard()) return;
  await api("updates", "install");
}

$("#update-now").onclick = busy($("#update-now"), restartForUpdate);
$("#dismiss-update").onclick = () => {
  dismissedUpdateVersion = updateNotice.dataset.version;
  updateNotice.hidden = true;
};

function paintUpdate(state, panel) {
  renderAbout(state, panel);
  const version = state.pendingVersion;
  if (!version || version === dismissedUpdateVersion) {
    updateNotice.hidden = true;
    return;
  }
  if (updateNotice.dataset.version !== version) {
    updateNotice.dataset.version = version;
    updateNotice.querySelector("[role=status]").textContent = tr("{version} güncellemesi hazır.", { version });
  }
  updateNotice.hidden = false;
}

function renderAbout(state, panel = $("#about-updates")) {
  if (!panel?.isConnected || !$("#dialog").open || panel.getAttribute("aria-busy") === "true") return;
  const snapshot = JSON.stringify(state);
  if (panel.dataset.snapshot === snapshot) return;
  panel.dataset.snapshot = snapshot;
  const run = async (action, args) => {
    panel.setAttribute("aria-busy", "true");
    panel.querySelectorAll("button, input").forEach(control => { control.disabled = true; });
    status.textContent = tr("Güncelleme işlemi devam ediyor…");
    try {
      await api("updates", action, args);
    } catch (e) {
      error(e);
    } finally {
      panel.removeAttribute("aria-busy");
      delete panel.dataset.snapshot;
      paintUpdate(await api("updates", "status"), panel);
    }
  };
  const installed = el("strong", "", tr("Kurulu sürüm {version}", { version: state.version }));
  const auto = el("input");
  auto.type = "checkbox";
  auto.checked = state.automatic;
  auto.disabled = !state.supported;
  auto.onchange = () => run("automatic", { enabled: auto.checked }).catch(error);
  const toggle = el("label", "update-toggle");
  toggle.append(auto, document.createTextNode(tr("Otomatik güncelle")));
  const help = el("p", "help", tr(state.supported
    ? "Workflow açıkken her gün kontrol eder. İndirilen güncelleme uygulama kapatıldığında kurulur."
    : "Otomatik güncelleme Windows x64 release derlemesinde kullanılabilir."));
  const status = el("p", "update-status");
  status.setAttribute("role", "status");
  const actions = el("div", "update-actions");
  actions.append(
    button(tr("Güncellemeleri kontrol et"), () => run("check")),
    button(tr("GitHub sürümleri"), () => api("updates", "open_releases"), "quiet"),
  );
  if (state.available?.downloadable && state.pendingVersion !== state.available.version) {
    actions.append(button(tr("Güncellemeyi indir"), () => run("download"), "primary"));
  }
  if (state.pendingVersion) {
    const install = button(tr("Güncelle ve yeniden başlat"), () => {}, "primary");
    install.onclick = busy(install, restartForUpdate);
    actions.append(install);
    status.textContent = tr("{version} güncellemesi hazır.", { version: state.pendingVersion });
  } else if (state.available) {
    status.textContent = tr("{version} güncellemesi var.", { version: state.available.version });
  } else if (state.checkedAt && !state.error) {
    status.textContent = tr("Daha yeni bir sürüm bulunamadı.");
  }
  panel.replaceChildren(installed, toggle, help, actions, status);
  if (state.pendingVersion) {
    panel.append(el("p", "help", tr("Yeniden başlatma açık oturumları durdurur.")));
  }
  if (state.error) panel.append(el("p", "dialog-error", tr(state.error)));
  if (state.error && Number.isFinite(state.httpStatus)) {
    panel.append(el("p", "help", tr("Sunucu yanıtı: HTTP {status}", { status: state.httpStatus })));
  }
  if (state.error && Number.isFinite(state.retryAt) && !Number.isNaN(new Date(state.retryAt * 1000).getTime())) {
    panel.append(el("p", "help", tr("Yeniden deneme zamanı: {time}", {
      time: new Date(state.retryAt * 1000).toLocaleString(uiLanguage),
    })));
  }

  if (state.checkedAt) {
    const time = new Date(state.checkedAt * 1000).toLocaleString(uiLanguage);
    panel.append(el("p", "help", tr("Son kontrol {time}", { time })));
  }
  const addNotes = (label, notes) => {
    if (!notes?.trim()) return;
    const section = el("section", "update-notes");
    section.append(el("strong", "", label), el("p", "", notes));
    panel.append(section);
  };
  addNotes(tr("Bu sürümdeki yenilikler"), state.installedNotes);
  if (state.available) {
    addNotes(tr("{version} sürümündeki yenilikler", { version: state.available.version }), state.available.notes);
  }
}
function panelToggle(side) {
  document.body.classList.toggle("no-" + side);
  remember();
  requestAnimationFrame(() => {
    fitTerminals();
    scheduleViewport();
  });
}
$("#toggle-left").onclick = () => panelToggle("left");
$("#toggle-right").onclick = () => panelToggle("right");
$("#panel-close").onclick = () => {
  document.body.classList.add("no-right");
  remember();
  hideBrowser();
  fitTerminals();
};
$("#panel-expand").onclick = () => {
  setPanelWidth("right", $("#layout").clientWidth * 0.65);
  savePanelWidths();
  fitTerminals();
  scheduleViewport();
};
const sidePanels = {
  left: { selector: ".sidebar", variable: "--sidebar" },
  right: { selector: ".inspector", variable: "--inspector" },
};
function setPanelWidth(side, width) {
  const layout = $("#layout"),
    style = getComputedStyle(layout),
    panel = sidePanels[side],
    other = sidePanels[side === "left" ? "right" : "left"],
    otherWidth = $(other.selector).getBoundingClientRect().width,
    fixedWidth = $(".project-rail").offsetWidth + $("#left-resizer").offsetWidth + $("#right-resizer").offsetWidth,
    minimum = parseFloat(style.getPropertyValue(panel.variable + "-min")),
    center = parseFloat(style.getPropertyValue("--main-min"));
  const root = document.documentElement.style;
  if (otherWidth) root.setProperty(other.variable, otherWidth + "px");
  root.setProperty(
    panel.variable,
    Math.max(
      minimum,
      Math.min(width, layout.clientWidth - center - otherWidth - fixedWidth),
    ) + "px",
  );
}
function savePanelWidths() {
  const style = getComputedStyle(document.documentElement);
  localStorage.setItem(
    "agentloom-widths",
    JSON.stringify({
      left: style.getPropertyValue("--sidebar"),
      right: style.getPropertyValue("--inspector"),
    }),
  );
}
function resizer(handle, move, done) {
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    hideBrowser();
    handle.setPointerCapture(e.pointerId);
    const start = { x: e.clientX, y: e.clientY };
    const fn = (event) => move(event, start);
    handle.addEventListener("pointermove", fn);
    const finish = () => {
      handle.removeEventListener("pointermove", fn);
      handle.removeEventListener("lostpointercapture", finish);
      done?.();
      fitTerminals();
      scheduleViewport();
    };
    handle.addEventListener("lostpointercapture", finish);
  });
}
for (const [side, panel] of Object.entries(sidePanels)) {
  const handle = $("#" + side + "-resizer");
  let initialWidth = 0;
  handle.addEventListener("pointerdown", () => {
    initialWidth = $(panel.selector).getBoundingClientRect().width;
  });
  resizer(
    handle,
    (event, start) => {
      const delta = event.clientX - start.x;
      setPanelWidth(side, initialWidth + (side === "left" ? delta : -delta));
    },
    savePanelWidths,
  );
}
for (const n of $$("[data-resize]")) {
  let height = 0;
  n.addEventListener(
    "pointerdown",
    () => (height = $("#" + n.dataset.resize).offsetHeight),
  );
  resizer(
    n,
    (e, start) => {
      const target = $("#" + n.dataset.resize);
      target.style.height =
        Math.max(
          55,
          Math.min(window.innerHeight * 0.75, height + e.clientY - start.y),
        ) + "px";
    },
    () => {
      const sections = {};
      $$("[data-resize]").forEach(
        (s) =>
          (sections[s.dataset.resize] = $("#" + s.dataset.resize).style.height),
      );
      localStorage.setItem("agentloom-sections", JSON.stringify(sections));
    },
  );
}

async function newProject() {
  const box = el("div"),
    nav = el("div", "row"),
    folder = button(
      tr("Klasörden ekle"),
      async () => {
        const directory = await chooseFolder();
        if (directory) {
          const p = await api("app", "add_project", { directory });
          closeModal();
          await sync();
          selectProject(p.id);
        }
      },
      "selected",
    ),
    clone = button(tr("Git’ten klonla"), () => {
      fields.hidden = false;
      clone.classList.add("selected");
      folder.classList.remove("selected");
    });
  nav.append(folder, clone);
  const fields = el("div");
  fields.hidden = true;
  fields.style.cssText =
    "display:flex;flex-direction:column;gap:12px;margin-top:15px";
  const address = input("", "https://github.com/…/repo.git"),
    parent = input(),
    name = input("", tr("Yeni klasör adı")),
    pick = button(tr("Konum seç"), async () => {
      parent.value = (await chooseFolder()) || parent.value;
    }),
    row = el("div", "row");
  parent.classList.add("grow");
  row.append(parent, pick);
  fields.append(
    field(tr("Git depo adresi"), address),
    field(tr("Kaydedilecek ana klasör"), row),
    field(tr("Yeni klasör adı"), name),
    el(
      "p",
      "help",
      tr("Bilgisayarındaki Git hesabı / SSH bağlantısı kullanılır. Adrese parola veya erişim anahtarı ekleme."),
    ),
  );
  box.append(nav, fields);
  modal(
    tr("Proje ekle"),
    box,
    async () => {
      if (fields.hidden) {
        const directory = await chooseFolder();
        if (directory) {
          const p = await api("app", "add_project", { directory });
          await sync();
          selectProject(p.id);
        }
      } else {
        const p = await api("app", "clone", {
          url: address.value,
          parent: parent.value,
          name: name.value,
        });
        await sync();
        selectProject(p.id);
      }
    },
    tr("Ekle"),
  );
  address.oninput = () => {
    if (!name.value)
      name.value = address.value
        .split("/")
        .pop()
        .replace(/\.git$/, "");
  };
}
function openProviderAccounts(provider) {
  const list = el("div", "provider-account-manager");
  const name = input("", tr("Örn. İş"));
  const connect = (accountId) => openProviderLogin(provider, accountId);
  const rows = el("div");
  list.append(rows);
  const changeMembership = async (accountId, removed) => {
    await api("app", removed ? "provider_account_remove" : "provider_account_restore", { provider, accountId });
    await scanProviders();
    await sync();
    if (list.isConnected && $("#dialog").open) paintRows();
    toast(tr(removed ? "Hesap Workflow’dan kaldırıldı. Geçmiş ve yerel dosyalar korundu." : "Hesap yeniden eklendi."));
  };
  const paintRows = () => {
    rows.replaceChildren();
    for (const account of providerInfo(provider).accounts) {
      const row = el("div", "row account-manager-row");
      row.dataset.accountId = account.id;
      const label = el("div", "grow");
      const running = db.sessions.some((session) => session.provider === provider &&
        session.accountId === account.id && active(session.id));
      label.append(
        el("strong", "", accountLabel(account)),
        el("small", "", account.id === defaultAccount
          ? tr("Bilgisayardaki mevcut sağlayıcı girişi")
          : running ? tr("Kaldırmak için bu hesaba bağlı çalışan oturumları durdur.")
          : account.verified ? tr("Hesap bağlandı") : tr("Giriş bekleniyor")),
      );
      row.append(label, button(
        account.verified ? tr("Girişi aç") : tr("Hesabı bağla"),
        () => connect(account.id),
      ));
      if (account.id !== defaultAccount) {
        const remove = button(tr("Kaldır"), () => {}, "quiet", tr("Hesabı Workflow’dan kaldır"));
        remove.disabled = running;
        remove.onclick = busy(remove, () => changeMembership(account.id, true));
        row.append(remove);
      }
      rows.append(row);
    }
    const removed = providerInfo(provider).removedAccounts || [];
    if (removed.length) {
      const details = el("details", "removed-provider-accounts");
      details.append(el("summary", "", tr("Kaldırılan hesaplar") + " · " + removed.length));
      for (const account of removed) {
        const row = el("div", "row account-manager-row");
        row.dataset.removedAccountId = account.id;
        const restore = button(tr("Geri ekle"), () => {});
        restore.onclick = busy(restore, () => changeMembership(account.id, false));
        row.append(el("strong", "grow", accountLabel(account)), restore);
        details.append(row);
      }
      rows.append(details);
    }
  };
  paintRows();
  const add = button(tr("Hesap ekle"), () => {}, "primary");
  add.onclick = busy(add, async () => {
    const err = list.querySelector(".dialog-error");
    err?.remove();
    try {
      const account = await api("app", "provider_account_add", { provider, name: name.value });
      providerInfo(provider).accounts.push(account);
      renderProviderCards();
      renderCards();
      if (list.isConnected && $("#dialog").open) await connect(account.id);
    } catch (e) {
      list.append(el("p", "dialog-error", uiError(e)));
    }
  });
  list.append(field(tr("Yeni hesap adı"), name), add,
    el("p", "help", tr("Kaldırılan hesapların oturum geçmişi ve yerel dosyaları korunur. Eski oturumlar başka bir hesaba geçirilmez.")));
  modal(names[provider] + tr(" · Hesaplar"), list);
}
async function openProviderLogin(provider, accountId = defaultAccount) {
  const dialog = $("#dialog");
  const notice = el(
    "p",
    "form-note",
    accountId === defaultAccount
      ? tr("Bu giriş Workflow’daki tüm projelerde kullanılabilir.")
      : tr("Sağlayıcının giriş ekranında bu profile bağlamak istediğin hesabı seç."),
  );
  const host = el("div", "terminal-host login-terminal");
  const status = el("p", "form-note", tr("Giriş ekranı açılıyor…"));
  status.setAttribute("role", "status");
  modal(names[provider] + tr(" · Hesap girişi"), [notice, host, status]);
  const close = $("#dialog-footer button");
  close.textContent = tr("Kapat");
  let runId, timer;
  let closed = false;
  let ended = false;
  let since = 0;
  const term = new Terminal({
    cursorBlink: true,
    theme: { background: "#0c1015", foreground: "#dbe4ee" },
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);
  terminalClipboard(term, host, () => !closed && !ended ? runId : null, (data) => {
    if (!/^\x1b\[[?0-9;]*[Rc]$/.test(data)) return call("write", { data });
  });
  fit.fit();
  const call = (action, args) =>
    api("provider_login", action, { runId, ...args });
  dialog.addEventListener(
    "close",
    () => {
      closed = true;
      clearTimeout(timer);
      term.dispose();
      if (runId) call("close").catch(error);
    },
    { once: true },
  );
  const poll = async () => {
    try {
      const result = await call("poll", { since });
      if (closed) return;
      since = result.seq;
      if (result.data) term.write(decode(result.data));
      if (result.exited) {
        ended = true;
        status.textContent = tr("Hesap durumu kontrol ediliyor…");
        if (accountId !== defaultAccount) {
          await api("app", "provider_account_verify", { provider, accountId });
          const account = providerAccount(provider, accountId);
          if (account) account.verified = true;
        }
        await usageInFlight.get(usageKey(provider, accountId))?.catch(() => {});
        await refreshProviderUsage(provider, accountId);
        if (closed) return;
        status.textContent =
          accountId !== defaultAccount ||
          providerAccount(provider, accountId)?.usage?.connection === "connected"
            ? tr("Hesap bağlı.")
            : tr("Hesap bağlantısı doğrulanamadı. Giriş ekranındaki sonucu kontrol et.");
        return;
      }
      timer = setTimeout(poll, 200);
    } catch (e) {
      if (!closed) status.textContent = uiError(e);
    }
  };
  try {
    ({ runId } = await api("provider_login", "start", { provider, accountId }));
    if (closed || !dialog.open) {
      await call("close");
      return;
    }
    await call("resize", { cols: term.cols, rows: term.rows });
    if (closed) return;
    status.textContent = tr("Sağlayıcının giriş adımlarını takip et.");
    term.focus();
    poll();
  } catch (e) {
    if (!closed) status.textContent = uiError(e);
  }
}
async function newSession(provider = "codex") {
  if (!db.projects.some((p) => !p.removed)) {
    await newProject();
    return;
  }
  const projects = select(
      db.projects
        .filter((p) => !p.removed)
        .map((p) => ({ id: p.id, label: p.name })),
      projectId,
    ),
    folders = select([], ""),
    providerChoice = el("div", "provider-choices"),
    accountChoice = select([]),
    name = input("", tr("Örn. API geliştirme")),
    workspaceMode = select(
      [
        { id: "isolated", label: tr("Ayrı çalışma alanı") },
        { id: "direct", label: tr("Mevcut klasör ve dal") },
      ],
      "isolated",
    ),
    workspaceNote = el("p", "form-note"),
    tools = check(tr("Oturum ve tarayıcı bağlantı araçları"), true),
    addonChoices = [];
  let selected = provider;
  function paint() {
    const saved = accountChoice.value;
    accountChoice.replaceChildren();
    const accounts = providerInfo(selected)?.accounts || [
      { id: defaultAccount, name: tr("Varsayılan hesap"), verified: true },
    ];
    for (const account of accounts) {
      const option = new Option(
        accountLabel(account) + (account.verified ? "" : tr(" · Giriş bekleniyor")),
        account.id,
      );
      option.disabled = !account.verified;
      accountChoice.add(option);
    }
    if (accounts.some((a) => a.id === saved && a.verified)) accountChoice.value = saved;
    providerChoice.replaceChildren(
      ...Object.keys(names).map((id) => {
        const b = button(
          "",
          () => {
            selected = id;
            paint();
          },
          id === selected ? "selected" : "",
        );
        b.append(
          el("span", "provider-icon " + id, icons[id]),
          el("span", "", names[id]),
        );
        return b;
      }),
    );
  }
  paint();
  const catalog = await api("addons", "list");
  const addonsBox = el("div");
  addonsBox.append(el("small", "", tr("Eklentiler · İsteğe bağlı")));
  for (const a of catalog) {
    const c = check(a.name);
    c.input.disabled = !a.installed;
    c.label.title = a.installed
      ? tMessage(a.description)
      : tr("Önce Eklentiler bölümünden kur");
    addonChoices.push([a.id, c.input]);
    addonsBox.append(c.label);
  }
  let workspaceRevision = 0;
  async function paintWorkspace() {
    const revision = ++workspaceRevision;
    const direct = workspaceMode.value === "direct";
    addonsBox.hidden = direct;
    workspaceNote.textContent = direct
      ? tr("Dosyalar doğrudan proje klasöründe değişir. Oturuma özel eklentiler için ayrı çalışma alanını seç.")
      : tr("Git deposunda ayrı bir worktree ve dal, Git yoksa ayrı bir klasör kopyası açılır.");
    if (!direct) return;
    try {
      const state = await api("git", "state", { projectId: projects.value });
      if (revision === workspaceRevision && state.available)
        workspaceNote.textContent =
          (state.branch === "HEAD" ? tr("Ayrık HEAD") : tr("Seçili dal ") + state.branch) +
          " · " + workspaceNote.textContent;
    } catch {
      // Session creation resolves the actual checkout from the selected folder.
    }
  }
  function paintFolders() {
    const options = folderOptions(projects.value);
    folders.replaceChildren(...options.map((option) => new Option(option.label, option.id)));
    folders.value = (projects.value === projectId ? folderId : knownFolder(projects.value, projectFolders[projects.value])) || "";
    folderField.hidden = options.length === 1;
  }
  const folderField = field(tr("Oturum klasörü"), folders);
  projects.onchange = () => {
    paintFolders();
    paintWorkspace();
  };
  workspaceMode.onchange = paintWorkspace;
  paintFolders();
  paintWorkspace();
  modal(
    tr("Yeni oturum"),
    [
      field(tr("Proje"), projects),
      folderField,
      providerChoice,
      field(tr("Hesap"), accountChoice),
      field(tr("Oturum adı"), name),
      field(tr("Çalışma alanı"), workspaceMode),
      workspaceNote,
      tools.label,
      addonsBox,
    ],
    async () => {
      const s = await api("app", "create_session", {
        projectId: projects.value,
        folderId: folders.value || null,
        provider: selected,
        accountId: accountChoice.value,
        name: name.value || names[selected] + tr(" · Oturum"),
        workspaceMode: workspaceMode.value,
        tools: tools.input.checked,
        addons:
          workspaceMode.value === "direct"
            ? []
            : addonChoices.filter(([, n]) => n.checked).map(([id]) => id),
      });
      await sync();
      selectSession(s.id);
      await toggleTerminal(s.id);
      setTimeout(fitMap, 250);
    },
    tr("Oturumu aç"),
  );
}
async function addonLibrary() {
  const catalog = await api("addons", "list");
  const box = el("div");
  for (const a of catalog) {
    const row = el("div", "provider-card");
    row.style.marginBottom = "12px";
    row.append(el("h3", "", a.name), el("p", "help", tMessage(a.description)));
    row.append(el("p", "help", a.previousVersion && !a.installed
      ? tr("Kurulu {previous} → doğrulanmış {version}", { previous: a.previousVersion.slice(0, 12), version: a.version })
      : tr("Sürüm {version}", { version: a.version })));
    row.append(
      button(
        a.installed ? tr("Kurulu · Yeniden kur") : a.previousVersion ? tr("Yükselt") : tr("Kur"),
        () => reviewAddonInstall(a),
        "primary",
      ),
    );
    box.append(row);
  }
  box.append(
    el(
      "p",
      "help",
      tr("Eklentileri oturum oluştururken veya oturumun ⋯ menüsünden etkinleştir."),
    ),
  );
  modal(tr("Eklenti kütüphanesi"), box);
}
function reviewAddonInstall(addon) {
  const box = el("div");
  box.append(el("p", "help", tr("Sürüm {version}", { version: addon.version })));
  const source = button(tr("Sürüm kaynağını aç"), () => api("app", "external", { url: addon.source }), "quiet");
  box.append(source, el("h3", "", tr("Oturuma eklenen dosyalar")));
  const list = el("ul");
  for (const file of addon.files) list.append(el("li", "", file));
  box.append(list);
  if (addon.command) {
    box.append(el("p", "help", tr("Kurulum, özeti doğrulanan Windows programını geçici çalışma alanında çalıştırır.")));
    box.append(el("code", "", addon.command));
  } else {
    box.append(el("p", "help", tr("Kurulum yalnızca doğrulanmış beceri belgelerini indirir.")));
  }
  if (addon.previousVersion && !addon.installed) {
    box.append(el("p", "help", tr("Bu sürüm kütüphanedeki eski sürümün yerini alır. Etkin oturumlar yeni dosyaları yeniden açıldığında alır.")));
    if (addon.changedFiles?.length) {
      box.append(el("h3", "", tr("Sürümde değişen dosyalar")));
      const changes = el("ul");
      const labels = { added: "Eklendi", modified: "Değişti", removed: "Kaldırıldı" };
      for (const change of addon.changedFiles)
        changes.append(el("li", "", change.file + " · " + tr(labels[change.status])));
      box.append(changes);
    } else if (!addon.changedFiles) {
      box.append(el("p", "help", tr("Eski kurulumun doğrulanmış dosya kaydı yok.")));
    }
  }
  closeModal();
  modal(addon.name, box, async () => {
    await api("addons", "install", { id: addon.id, revision: addon.revision });
    toast(addon.name + tr(" kütüphaneye kuruldu"));
  }, addon.previousVersion && !addon.installed ? tr("Yükselt") : tr("Kur"));
}
async function sessionAddons(id) {
  const s = session(id),
    catalog = await api("addons", "list"),
    box = el("div"),
    choices = [];
  for (const a of catalog) {
    const c = check(a.name, (s.addons || []).includes(a.id));
    c.input.disabled = !a.installed;
    choices.push([a.id, c.input]);
    box.append(c.label);
  }
  box.append(
    el(
      "p",
      "help",
      tr("Seçim bu oturumun ayrı çalışma alanına uygulanır. Açık terminal varsa önce durdur."),
    ),
  );
  modal(tr("Oturum eklentileri"), box, async () => {
    await api("addons", "select", {
      id,
      addons: choices.filter(([, c]) => c.checked).map(([id]) => id),
    });
    await sync();
  });
}
function archiveList() {
  const box = el("div");
  box.id = "session-history";
  const projects = new Map(db.projects.map((p) => [p.id, p]));
  const running = new Set(db.active.map((run) => run.id));
  const order = (s) => s.lastUsedAt || s.createdAt || "";
  const sessions = [...db.sessions].sort((a, b) => order(b).localeCompare(order(a)));
  const group = (label, rows, empty, expanded = true) => {
    const section = el("details", "session-history-group");
    section.open = expanded;
    const heading = el("summary", "", tr(label));
    heading.append(el("span", "folder-count", rows.length));
    section.append(heading);
    if (!rows.length) section.append(el("p", "help", tr(empty)));
    return section;
  };
  const sessionRow = (s, grouped = false) => {
    const owner = projects.get(s.projectId);
    const row = el("div", "archive-row");
    row.dataset.sessionId = s.id;
    const title = el("span", "archive-session-label");
    const context = grouped ? names[s.provider] : [owner?.name, names[s.provider]].filter(Boolean).join(" · ");
    title.append(el("strong", "", s.name), el("small", "", context));
    const action = button(tr(s.archived ? "Geri getir" : "Arşive taşı"), () => {});
    action.onclick = busy(action, async () => {
      if (!s.archived) {
        await archiveSession(s.id, true);
        return;
      }
      if (owner?.removed) await api("app", "add_project", { directory: owner.directory });
      await api("app", "session_patch", { id: s.id, archived: false });
      closeModal();
      await sync();
      selectSession(s.id);
    });
    row.append(title, button(tr("Geçmiş"), () => showTranscript(s.id, true)), action);
    if (hasUnmovedArchive(s)) {
      const move = button(tr("Klasörü arşive taşı"), () => {}, "legacy-archive-move");
      move.onclick = busy(move, () => archiveSession(s.id, true));
      row.append(move);
    }
    return row;
  };
  const archived = sessions.filter((s) => s.archived);
  const archive = group("Arşiv", archived, "Arşivlenmiş oturum yok.");
  archive.dataset.historyGroup = "archive";
  const byFolder = new Map();
  for (const s of archived) {
    const key = folderLayoutKey(s.projectId, s.folderId || null);
    if (!byFolder.has(key)) byFolder.set(key, []);
    byFolder.get(key).push(s);
  }
  for (const members of byFolder.values()) {
    const first = members[0];
    const folder = folderOptions(first.projectId).find((item) => item.id === (first.folderId || ""));
    const name = folder?.label || tr("Genel");
    const owner = projects.get(first.projectId);
    const detail = el("details", "archive-folder-group");
    detail.open = true;
    const heading = el("summary", "", [owner?.name, name].filter(Boolean).join(" · "));
    heading.append(el("span", "folder-count", members.length));
    detail.append(heading,
      button(tr("Klasörü geri getir"), () => folderArchiveDialog(name, members, false), "archive-folder-restore"),
      ...members.map((s) => sessionRow(s, true)));
    archive.append(detail);
  }
  const closed = sessions.filter((s) => !s.archived && !running.has(s.id));
  const closedGroup = group("Kapalı oturumlar", closed, "Kapalı oturum yok.");
  closedGroup.append(...closed.map((s) => sessionRow(s)));
  const open = sessions.filter((s) => !s.archived && running.has(s.id));
  box.append(archive, closedGroup);
  if (open.length) {
    const openGroup = group("Açık oturumlar", open, "", false);
    openGroup.append(...open.map((s) => sessionRow(s)));
    box.append(openGroup);
  }
  modal(tr("Oturum geçmişi"), box);
}
function editTemplate(entry, projectId, destinationId) {
  const template = entry.template;
  const content = el("div", "setup-content");
  const name = input(template.name);
  name.maxLength = 160;
  const fromMap = check(tr("Oturumları ve bağlantıları mevcut haritadan al"));
  fromMap.input.disabled = !visibleSessions().length;
  const editor = el("div", "setup-list");
  const rows = template.sessions.map((session, index) => {
    const section = el("details", "setup-row");
    const heading = el("summary", "", session.name);
    const sessionName = input(session.name);
    sessionName.maxLength = 160;
    sessionName.oninput = () => { heading.textContent = sessionName.value || session.name; };
    const provider = select(Object.entries(names).map(([id, label]) => ({ id, label })), session.provider);
    const workspace = select([
      { id: "isolated", label: tr("Ayrı klasör") },
      { id: "direct", label: tr("Mevcut klasör") },
    ], session.workspaceMode);
    const model = input(session.model || "", tr("Varsayılan model"));
    model.maxLength = 128;
    provider.onchange = () => { model.value = provider.value === session.provider ? session.model || "" : ""; };
    const prompt = el("textarea");
    prompt.rows = 3;
    prompt.maxLength = 4000;
    prompt.value = session.initialPrompt || "";
    section.append(heading, field(tr("Oturum adı"), sessionName), field(tr("Sağlayıcı"), provider),
      field(tr("Çalışma alanı"), workspace), field(tr("Model"), model), field(tr("Şablon istemi"), prompt));
    if (!index) section.open = true;
    editor.append(section);
    return () => ({
      name: sessionName.value.trim(), provider: provider.value, workspaceMode: workspace.value,
      model: model.value.trim() || null, initialPrompt: prompt.value || null, position: session.position,
    });
  });
  const permissions = template.links.map((link) => {
    const section = el("details", "setup-row");
    const read = check(tr("Konuşma"), link.read);
    const files = check(tr("Dosya"), link.files);
    const work = check(tr("İş"), link.work);
    section.append(el("summary", "", template.sessions[link.source].name + " → " + template.sessions[link.target].name),
      read.label, files.label, work.label);
    editor.append(section);
    return () => ({ ...link, read: read.input.checked, files: files.input.checked, work: work.input.checked });
  });
  fromMap.input.onchange = () => { editor.hidden = fromMap.input.checked; };
  content.append(field(tr("Şablon adı"), name), fromMap.label, editor);
  modal(tr("Şablonu düzenle"), content, async () => {
    const update = fromMap.input.checked
      ? { projectId, folderId, name: name.value }
      : { template: { ...template, name: name.value.trim(), sessions: rows.map((row) => row()), links: permissions.map((read) => read()) } };
    await api("templates", "update", { id: entry.id, revision: entry.revision, ...update });
    $("#dialog").addEventListener("close", () => openTemplates(entry.id, destinationId).catch(error), { once: true });
  });
  $("#dialog-footer button").onclick = () => openTemplates(entry.id, destinationId).catch(error);
  name.focus();
}

function templateTitle(entry) {
  const builtInTitles = {
    "builtin:0": "Kod + İnceleme",
    "builtin:1": "Özellik geliştirme",
    "builtin:2": "Hata ayıklama",
    "builtin:3": "Güvenlik Taraması",
  };
  return uiLanguage === "tr" && Object.hasOwn(builtInTitles, entry.id)
    ? builtInTitles[entry.id] : entry.template.name;
}

async function openTemplates(selectedId, destinationId = projectId) {
  const entries = await api("templates", "list");
  const templateSelect = select(entries.map((entry) => ({ id: entry.id, label: templateTitle(entry) })), selectedId);
  const projects = db.projects.filter((p) => !p.removed);
  const destination = select(projects.map((p) => ({ id: p.id, label: p.name })), destinationId);
  const content = el("div", "setup-content");
  const preview = el("div", "setup-list");
  const installed = Object.keys(names).filter((id) => providerInfo(id)?.installed);
  let choices = [];
  const selection = () => entries.find((entry) => entry.id === templateSelect.value);
  const edit = button(tr("Şablonu düzenle"), () => editTemplate(selection(), projectId, destination.value));
  const remove = button(tr("Şablonu sil"), () => {
    const entry = selection();
    const target = destination.value;
    modal(tr("Şablonu sil"), el("p", "", tr("“{name}” şablonu silinsin mi?", { name: entry.template.name })), async () => {
      await api("templates", "delete", { id: entry.id, revision: entry.revision });
      $("#dialog").addEventListener("close", () => openTemplates(undefined, target).catch(error), { once: true });
    }, tr("Sil"));
    $("#dialog-footer button").onclick = () => openTemplates(entry.id, target).catch(error);
    $("#dialog-footer button").focus();
  }, "danger");
  const render = () => {
    const entry = selection();
    const { template } = entry;
    edit.hidden = remove.hidden = !entry.revision;
    const options = [
      { id: "", label: tr("Sağlayıcı seç") },
      ...installed.map((id) => ({ id, label: names[id] })),
    ];
    choices = [];
    preview.replaceChildren(...template.sessions.map((session, index) => {
      const saved = entry.selectedProviders?.[index];
      const provider = Object.hasOwn(names, saved) ? saved : session.provider;
      const available = installed.includes(provider);
      const choice = select(options, available ? provider : "");
      choices.push(choice);
      const label = session.name + " · " + tr(session.workspaceMode === "direct" ? "Mevcut klasör" : "Ayrı klasör");
      const row = el("section", "setup-row");
      row.append(field(label, choice));
      if (!available)
        row.append(el("small", "link-reapproval", tr("{provider} kurulu değil. Başka bir sağlayıcı seç.", { provider: names[provider] })));
      return row;
    }));
    if (template.links.length) {
      const links = el("ul", "template-links");
      for (const link of template.links) links.append(el("li", "",
        template.sessions[link.source].name + " → " + template.sessions[link.target].name +
        " · " + linkPermissionNames(link).join(" · ")));
      preview.append(links);
    }
    const apply = $("#dialog-footer .primary");
    const validateSelection = () => { apply.disabled = !destination.value || choices.some((choice) => !choice.value); };
    choices.forEach((choice) => { choice.onchange = validateSelection; });
    destination.onchange = validateSelection;
    validateSelection();
  };
  const actions = el("div", "row template-actions");
  const capture = button(tr("Haritayı şablon olarak kaydet"), () => {
    const name = input("", tr("Şablon adı"));
    modal(tr("Haritayı şablon olarak kaydet"), field(tr("Şablon adı"), name), async () => {
      const entry = await api("templates", "capture", { projectId, folderId, name: name.value });
      $("#dialog").addEventListener("close", () => openTemplates(entry.id).catch(error), { once: true });
    });
    name.focus();
  });
  capture.disabled = visibleSessions().length === 0;
  const chooseTemplateFile = (action) => invoke("plugin:dialog|" + action, {
    options: { filters: [{ name: "JSON", extensions: ["json"] }],
      ...(action === "save" ? { defaultPath: "workflow-template.json" } : {}) },
  });
  actions.append(capture,
    button(tr("İçe aktar"), async () => {
      const path = await chooseTemplateFile("open");
      if (path) {
        const entry = await api("templates", "import", { path });
        await openTemplates(entry.id);
      }
    }),
    button(tr("Dışa aktar"), async () => {
      const id = selection().id;
      const path = await chooseTemplateFile("save");
      if (path) {
        await api("templates", "export", { id, path });
        toast(tr("Şablon dışa aktarıldı."));
      }
    }),
    edit, remove,
  );
  content.append(actions, field(tr("Şablon"), templateSelect), field(tr("Hedef proje"), destination), preview);
  if (!projects.length) content.append(el("p", "help", tr("Şablonu uygulamadan önce bir proje ekle.")));
  content.append(el("p", "help", tr("Oturumlar ve bağlantılar oluşturulur. Oturumları haritadan başlatabilirsin.")));
  modal(tr("İş akışı şablonları"), content, async () => {
    const target = destination.value;
    const targetFolder = target === projectId ? folderId : knownFolder(target, projectFolders[target]);
    await api("templates", "apply", { id: selection().id, projectId: target, folderId: targetFolder, providers: choices.map((choice) => choice.value) });
    await sync();
    selectProject(target);
    setMode("map");
    fitMap();
  }, tr("Oturumları oluştur"));
  templateSelect.onchange = render;
  render();
}

$("#new-session").onclick = () => newSession().catch(error);
const templateMenu = $("#session-template-menu");
const templateItems = $("#session-template-items");
function positionTemplateMenu() {
  const { left, bottom, width } = $(".sidebar-actions").getBoundingClientRect();
  const top = bottom + 5;
  templateMenu.style.left = left + "px";
  templateMenu.style.top = top + "px";
  templateMenu.style.width = width + "px";
  templateMenu.style.maxHeight = Math.min(280, innerHeight - top - 8) + "px";
}
templateMenu.addEventListener("beforetoggle", (event) => {
  if (event.newState === "open") positionTemplateMenu();
});
templateMenu.addEventListener("toggle", async (event) => {
  if (event.newState !== "open") return;
  const loading = el("p", "muted", tr("Yükleniyor…"));
  templateItems.replaceChildren(loading);
  try {
    const entries = await api("templates", "list");
    if (loading.parentElement !== templateItems) return;
    templateItems.replaceChildren(...entries.map((entry) => {
      const label = templateTitle(entry);
      const item = button(label, () => {
        templateMenu.hidePopover();
        return openTemplates(entry.id);
      }, "", label);
      item.dataset.templateId = entry.id;
      return item;
    }));
  } catch (e) {
    if (loading.parentElement === templateItems) {
      loading.remove();
      error(e);
    }
  }
});
$("#manage-session-templates").onclick = () => {
  templateMenu.hidePopover();
  openTemplates().catch(error);
};
$("#add-project").onclick = () => newProject().catch(error);
$("#addons").onclick = () => addonLibrary().catch(error);
$("#archive").onclick = archiveList;
$("#refresh").onclick = busy($("#refresh"), async () => {
  await scanProviders();
  await sync();
  await refreshPanel();
});
$("#show-providers").onclick = () => {
  const section = $("#providers-section");
  section.hidden = !section.hidden;
  $("#show-providers").setAttribute("aria-expanded", String(!section.hidden));
  remember();
  refreshProviderUsageAll().catch(() => {});
  fitTerminals();
};

async function openPanel(tab, reveal = true) {
  if (tab === "git") {
    changesPreference = "git";
    changesHistory = false;
    tab = "changes";
  }
  panel = tab;
  if (reveal) document.body.classList.remove("no-right");
  for (const n of ["browser", "files", "changes", "links", "summary"])
    $("#panel-" + n).hidden = n !== tab;
  $$("#panel-tabs button").forEach((b) =>
    b.classList.toggle("selected", b.dataset.tab === tab),
  );
  $("#panel-title").textContent = {
    browser: tr("Tarayıcı"),
    files: tr("Dosyalar"),
    changes: tr("Değişiklikler"),
    links: tr("Oturum Bağlantıları"),
    summary: tr("Oturum Özeti"),
  }[tab];
  drawEdges();
  if (tab !== "browser") hideBrowser();
  await refreshPanel();
  remember();
  fitTerminals();
}
$$("#panel-tabs button").forEach(
  (b) => (b.onclick = () => openPanel(b.dataset.tab).catch(error)),
);
$("#panel-refresh").onclick = () => refreshPanel().catch(error);
async function refreshPanel() {
  const token = ++panelGeneration;
  $("#panel-context").textContent =
    (project()?.name || "") +
    (session(focusId) ? " · " + session(focusId).name : "");
  if (panel !== "browser") hideBrowser();
  if (panel === "summary") return loadSummary(token);
  if (!projectId && !["files", "changes"].includes(panel)) return;
  try {
    if (panel === "files") await loadFiles(token);
    if (panel === "changes") await loadChangesPanel(token);
    if (panel === "links") renderLinks();
    if (panel === "browser") await loadBrowser();
  } catch (e) {
    if (token === panelGeneration) error(e);
  }
}
function renderSummaryHeader(s) {
  $("#summary-name").textContent = s.name;
  $("#summary-meta").textContent = [
    names[s.provider],
    status(s.id).label,
    s.lastUsedAt
      ? tr("Son başlatma ") + new Date(s.lastUsedAt).toLocaleString(uiLocale)
      : tr("Henüz başlatılmadı"),
  ].join(" · ");
  const rows = db.links
    .filter((link) => link.source === s.id || link.target === s.id)
    .map((link) => {
      const outgoing = link.source === s.id;
      const other = session(outgoing ? link.target : link.source);
      return button(
        (outgoing ? "→ " : "← ") + (other?.name || tr("Oturum")),
        () => openLink(link.source, link.target),
        "summary-link",
      );
    });
  $("#summary-links").replaceChildren(
    ...(rows.length ? rows : [el("p", "help", tr("Bağlı oturum yok."))]),
  );
}
async function loadSummary(token) {
  const s = session(focusId);
  const hasSession = !!s && !s.login;
  $("#summary-empty").hidden = hasSession;
  $("#summary-content").hidden = !hasSession;
  if (!hasSession) return;

  renderSummaryHeader(s);
  const files = $("#summary-files"),
    count = $("#summary-file-count"),
    diff = $("#summary-diff");
  diff.hidden = true;
  diff.replaceChildren();
  count.textContent = tr("Dosya farkı");
  $("#summary-file-note").textContent =
    s.workspaceKind === "direct"
      ? tr("Başlangıca göre net fark. Ortak klasördeki tüm düzenlemeler dahildir.")
      : tr("Oturumun ilk açıldığı andan bugüne kalan net fark.");
  files.replaceChildren(el("p", "help", tr("Dosyalar karşılaştırılıyor…")));
  $("#summary-trace").replaceChildren();
  $("#summary-trace-count").textContent = tr("Tarayıcı adımları yükleniyor…");
  $("#summary-all-trace").disabled = true;
  $("#summary-all-trace").onclick = () => openTrace(s.id).catch(error);
  const trace = loadSummaryTrace(s.id, token);
  try {
    const rows = await api("files", "changes", { sessionId: s.id });
    if (token !== panelGeneration) return;
    count.textContent = countLabel(rows.length, "{count} dosya değişti");
    files.replaceChildren(
      ...rows.map((item) => {
        const row = button(
          "",
          () => showSummaryDiff(s.id, item.path, row, token),
          "file-row",
        );
        row.setAttribute("aria-pressed", "false");
        row.title = item.path;
        row.append(el("span", "status", item.status), el("code", "", item.path));
        return row;
      }),
    );
    if (!rows.length)
      files.append(el("p", "help", tr("Başlangıç kaydına göre değişiklik yok.")));
  } catch (e) {
    if (token === panelGeneration)
      files.replaceChildren(el("p", "help", tr("Dosya farkı alınamadı. ") + uiError(e)));
  }
  await trace;
}
async function showSummaryDiff(sid, path, row, token) {
  const revision = ++summaryDiffRevision;
  const diff = $("#summary-diff");
  for (const item of $("#summary-files").children)
    item.setAttribute("aria-pressed", String(item === row));
  diff.hidden = false;
  diff.replaceChildren(el("h4", "", path), el("p", "help", tr("Fark yükleniyor…")));
  const current = () =>
    token === panelGeneration && revision === summaryDiffRevision;
  try {
    const result = await api("files", "diff", { sessionId: sid, path });
    if (!current()) return;
    const body = el("div", "diff-view");
    renderDiff(body, result, null, true);
    diff.replaceChildren(el("h4", "", path), body,
      button(tr("Değişikliklerde aç"), async () => {
        changesPreference = "session";
        changesHistory = false;
        changeSelection = { kind: "session", path, scope: sid };
        await openPanel("changes");
      }, "quiet"));
  } catch (e) {
    if (current())
      diff.replaceChildren(
        el("h4", "", path),
        el("p", "help", tr("Fark alınamadı. ") + uiError(e)),
      );
  }
}
async function loadFiles(token = panelGeneration) {
  const context = ctx();
  const scope = context.sessionId || context.projectId;
  if (scope !== fileScope) fileDirectory = "";
  fileScope = scope;
  fileSelection = null;
  ++fileReadRevision;
  $("#file-edit").hidden = true;
  $("#file-title").textContent = tr("Bir dosya seç");
  $("#file-preview").replaceChildren();
  $("#file-path").textContent = context.projectId ? fileDirectory || "/" : "";
  $("#file-list").replaceChildren();
  if (!context.projectId) {
    $("#file-list").append(el("p", "empty", tr("Bir proje seç.")));
    return;
  }
  const entries = await api("files", "list", { ...context, path: fileDirectory });
  if (token !== panelGeneration) return;
  for (const e of entries) {
    const row = button("", async () => {
      if (e.directory) {
        fileDirectory = e.path;
        await refreshPanel();
      } else {
        await showFile({ ...context, path: e.path }, token);
      }
    }, "file-row");
    row.append(
      el("span", "status", e.directory ? "▱" : "≡"),
      el("code", "", e.name),
    );
    $("#file-list").append(row);
  }
}
async function showFile(target, token = panelGeneration) {
  const revision = ++fileReadRevision;
  const current = () => token === panelGeneration && revision === fileReadRevision;
  fileSelection = null;
  $("#file-edit").hidden = true;
  $("#file-title").textContent = target.path;
  $("#file-preview").replaceChildren(el("p", "help", tr("Dosya yükleniyor…")));
  try {
    const result = await api("files", "read", target);
    if (!current()) return;
    if (result.image) {
      const img = el("img");
      img.src = result.image;
      img.style.maxWidth = "100%";
      $("#file-preview").replaceChildren(img);
    } else {
      $("#file-preview").replaceChildren(el("pre", "", result.text));
      fileSelection = target;
      $("#file-edit").hidden = false;
    }
  } catch (e) {
    if (current()) $("#file-preview").replaceChildren(el("p", "help", uiError(e)));
  }
}
$("#file-edit").onclick = busy($("#file-edit"), async () => {
  const target = fileSelection;
  const token = panelGeneration;
  if (!target) return;
  const source = await api("files", "edit_read", target);
  if (fileSelection !== target || token !== panelGeneration || $("#dialog").open) return;
  const s = session(target.sessionId);
  const destination = s && s.workspaceKind !== "direct"
    ? tr("Ayrı oturum çalışma alanı · ") + s.name
    : tr("Proje klasörü · ") + project()?.name;
  const editor = el("textarea", "file-editor");
  editor.value = source.text;
  editor.spellcheck = false;
  editor.wrap = "off";
  editor.setAttribute("aria-label", tr("Dosya içeriği"));
  const leave = el("div", "row file-editor-leave");
  leave.hidden = true;
  const keep = button(tr("Düzenlemeye dön"), () => {
    leave.hidden = true;
    editor.focus();
  });
  leave.append(
    el("span", "grow", tr("Kaydedilmemiş düzenlemeler var.")),
    keep,
    button(tr("Taslağı sil"), () => {
      dialogCloseGuard = null;
      closeModal();
    }),
  );
  let saving = false;
  modal(tr("Dosyayı düzenle"), [
    el("div", "file-editor-location", target.path + " · " + destination),
    editor,
    leave,
  ], async () => {
    leave.hidden = true;
    saving = editor.readOnly = true;
    try {
      await api("files", "edit_save", { ...target, revision: source.revision, text: editor.value });
      dialogCloseGuard = null;
      toast(tr("Dosya kaydedildi."));
      if (fileSelection === target && token === panelGeneration) showFile(target, token);
    } finally {
      saving = editor.readOnly = false;
    }
  });
  dialogCloseGuard = () => {
    if (saving) return false;
    if (editor.value !== source.text) {
      leave.hidden = false;
      keep.focus();
      return false;
    }
    dialogCloseGuard = null;
    return true;
  };
  const footer = $("#dialog-footer");
  footer.prepend(button(tr("Taslağı kopyala"), async () => {
    await navigator.clipboard.writeText(editor.value);
    toast(tr("Taslak kopyalandı."));
  }, "quiet"));
  editor.focus();
});
$("#files-up").onclick = () => {
  fileDirectory = fileDirectory.split("/").slice(0, -1).join("/");
  refreshPanel().catch(error);
};
$("#files-root").onclick = () => {
  fileDirectory = "";
  refreshPanel().catch(error);
};
$("#files-reveal").onclick = () => api("app", "reveal", ctx()).catch(error);
function setChangesMode(mode) {
  if (mode !== "history") changesPreference = mode;
  changesHistory = mode === "history";
  remember();
  return refreshPanel();
}
for (const mode of ["session", "git", "history"])
  $("#changes-mode-" + mode).onclick = () => setChangesMode(mode);

function showCommitForm(open, focus = false) {
  $("#git-commit-section").hidden = !open;
  $("#show-commit").setAttribute("aria-expanded", String(open));
  if (open && focus) $("#commit-message").focus();
}
$("#show-commit").onclick = () => showCommitForm($("#git-commit-section").hidden, true);

function clearReview(message = "") {
  changeRevision++;
  changeDiff = null;
  $("#change-review").hidden = !message;
  $("#change-file").textContent = "";
  $("#change-comparison").textContent = "";
  $("#change-stats").hidden = true;
  $("#change-restore").hidden = true;
  $("#restore-file").disabled = $("#restore-lines").disabled = true;
  $("#change-diff").replaceChildren(...(message ? [el("p", "help", message)] : []));
}

async function loadChangesPanel(token) {
  const context = ctx(), sid = focusId;
  const scope = sid || projectId || "";
  const scopeChanged = changesScope !== scope;
  if (scopeChanged) {
    changesScope = scope;
    if (changeSelection?.scope !== scope) changeSelection = null;
    selectedCommit = baseCommit = null;
    lastTransfer = null;
    gitSelected.clear();
    $("#commit-message").value = "";
    $("#history-branch").value = "--all";
  }
  const commitFormWasOpen = !scopeChanged && !$("#git-commit-section").hidden;
  clearReview();
  showCommitForm(false);
  changesMode = null;
  gitState = null;
  for (const id of ["changes-session-actions", "changes-git-actions",
    "changes-history-actions", "change-files", "changes-mode-git",
    "changes-mode-session", "changes-mode-history"])
    $("#" + id).hidden = true;
  const empty = $("#changes-empty");
  empty.hidden = false;
  empty.textContent = tr(projectId ? "Değişiklikler yükleniyor…" : "Bir proje seç.");
  if (!projectId) return;
  const state = await api("git", "state", { ...context, reviewStats: true });
  if (token !== panelGeneration) return;
  gitState = state;
  changesMode = state.available
    ? changesHistory ? "history" : sid ? changesPreference : "git"
    : sid ? "session" : null;
  for (const [mode, available] of [
    ["session", !!sid], ["git", state.available], ["history", state.available],
  ]) {
    const control = $("#changes-mode-" + mode);
    control.hidden = !available;
    control.setAttribute("aria-pressed", String(changesMode === mode));
  }
  empty.hidden = !!changesMode;
  empty.textContent = tr("Oturum farkını görmek için bir oturum seç.");
  if (!changesMode) return;
  $("#change-files").hidden = false;
  $("#change-list").hidden = changesMode !== "session";
  $("#git-lists").hidden = changesMode !== "git";
  $("#history-lists").hidden = changesMode !== "history";
  if (changesMode === "session") await loadSessionReview(context, sid, token);
  else if (changesMode === "git") {
    $("#changes-git-actions").hidden = false;
    showCommitForm(commitFormWasOpen);
    await loadGitReview(context, state, token);
  } else {
    $("#changes-history-actions").hidden = false;
    const choice = $("#history-branch"), value = choice.value;
    choice.replaceChildren(new Option(tr("Tüm dallar"), "--all"),
      ...state.branches.map((b) => new Option(b.name, b.name)));
    choice.value = [...choice.options].some((o) => o.value === value) ? value : "--all";
    await loadHistory(context, token);
  }
}

function reviewFileRow(item, open) {
  const row = el("div", "file-row");
  const path = button("", () => open(row), "change-path", item.path);
  path.setAttribute("aria-pressed", "false");
  path.append(el("span", "status", item.status), el("code", "", item.path));
  if (item.stats) path.append(diffStatsLabel(item.stats));
  row.append(path);
  return row;
}

async function showReview(selection, row, comparison, load, token = panelGeneration) {
  const path = selection.path;
  if (token !== panelGeneration) return;
  clearReview(tr("Fark yükleniyor…"));
  const revision = changeRevision;
  const current = () => token === panelGeneration && revision === changeRevision;
  changeSelection = { ...selection, scope: changesScope };
  for (const item of $$("#change-files .file-row.active")) {
    item.classList.remove("active");
    item.querySelector(".change-path")?.setAttribute("aria-pressed", "false");
  }
  row.classList.add("active");
  row.querySelector(".change-path").setAttribute("aria-pressed", "true");
  $("#change-file").textContent = path;
  $("#change-comparison").textContent = comparison;
  try {
    const response = await load();
    if (!current()) return;
    const result = response.lines || response.binary ? response : reviewPatch(response.patch ?? "");
    if (!result.binary && result.lines?.length) {
      const stats = reviewStats(result.lines);
      $("#change-stats").replaceChildren(diffStatsLabel(stats));
      $("#change-stats").hidden = false;
      const label = row.querySelector(".change-stats") || el("span");
      if (!label.parentElement) row.querySelector(".change-path").append(label);
      label.replaceWith(diffStatsLabel(stats));
    }
    const editable = selection.kind === "session";
    $("#change-restore").hidden = !editable;
    $("#restore-file").disabled = !editable;
    changeDiff = result;
    renderDiff($("#change-diff"), result, editable ? () => {
      $("#restore-lines").disabled = !$$("#change-diff input:checked").length;
    } : null);
  } catch (e) {
    if (current()) $("#change-diff").replaceChildren(
      el("p", "help", tr("Fark alınamadı. ") + uiError(e)),
    );
  }
}

async function loadSessionReview(context, sid, token) {
  const direct = session(sid)?.workspaceKind === "direct";
  const list = $("#change-list");
  list.replaceChildren(el("p", "help", tr("Dosyalar karşılaştırılıyor…")));
  $("#transfer-open").hidden = $("#transfer-undo").hidden = direct;
  $("#restore-undo").disabled = !lastRestore || lastRestore.sessionId !== sid;
  const [rows, transfers] = await Promise.all([
    api("files", "changes", { ...context, reviewStats: true }),
    direct ? [] : api("files", "transfer_history", context),
  ]);
  if (token !== panelGeneration) return;
  lastTransfer =
    transfers.find((t) => ["applied", "prepared", "reverting"].includes(t.state)) ||
    null;
  $("#transfer-undo").disabled = !lastTransfer;
  $("#changes-session-actions").hidden = false;
  list.replaceChildren();
  const comparison = tr(direct
    ? "Oturum başlangıcı → Mevcut dosyalar · Ortak klasördeki tüm düzenlemeler"
    : "Oturum başlangıcı → Mevcut dosyalar");
  const entries = [];
  for (const item of rows) {
    const open = (row) => showReview({ kind: "session", path: item.path },
      row, comparison, () => api("files", "diff", { ...context, path: item.path }), token);
    const row = reviewFileRow(item, open);
    entries.push({ item, row, open });
    list.append(row);
  }
  await openReviewEntry(entries, "session");
  if (!rows.length)
    list.append(el("p", "empty", tr("Başlangıç kaydına göre değişiklik yok.")));
}

async function openReviewEntry(entries, kind) {
  const entry = entries.find(({ item }) => changeSelection?.kind === kind &&
    changeSelection.path === item.path && changeSelection.staged === item.staged)
    ?? entries.find(({ item }) => !item.conflict);
  if (entry) await entry.open(entry.row);
}
async function restore(selected) {
  if (changeSelection?.kind !== "session" || !changeDiff) return;
  const indices = selected
    ? $$("#change-diff input:checked").map((c) => Number(c.dataset.index))
    : undefined;
  const context = ctx(),
    path = changeSelection.path,
    stamp = changeDiff.stamp;
  modal(
    selected ? tr("Seçili satırları geri al") : tr("Dosyayı geri al"),
    el(
      "p",
      "help",
      path +
        tr(" başlangıç durumuna göre geri alınacak. Mevcut içerik geri alma kaydında korunur."),
    ),
    async () => {
      lastRestore = await api("files", "restore", {
        ...context,
        path,
        stamp,
        indices,
      });
      $("#restore-undo").disabled = false;
      await refreshChanges();
    },
    tr("Geri al"),
  );
}
const refreshChanges = () => panel === "changes" ? refreshPanel() : Promise.resolve();
$("#restore-file").onclick = () => restore(false);
$("#restore-lines").onclick = () => restore(true);
$("#restore-undo").onclick = async () => {
  try {
    await api("files", "undo_restore", { ...ctx(), token: lastRestore.id });
    $("#restore-undo").disabled = true;
    await refreshChanges();
  } catch (e) {
    error(e);
  }
};
async function transferDialog() {
  if (!focusId) throw Error(tr("Bir oturum seç"));
  const context = ctx(),
    items = await api("files", "transfer_list", context),
    box = el("div"),
    choices = [];
  for (const item of items) {
    const row = el("div", "provider-card");
    row.style.marginBottom = "8px";
    const c = check(item.path, true),
      resolution = select([
        { id: "", label: tr("Çözüm seç") },
        { id: "main", label: tr("Ana projeyi koru") },
        { id: "session", label: tr("Oturumdaki dosyayı kullan") },
        { id: "manual", label: tr("Elle düzenle") },
      ]);
    resolution.hidden = !item.conflict;
    const manual = el("textarea");
    manual.hidden = true;
    manual.style.cssText = "width:100%;height:150px";
    resolution.onchange = async () => {
      manual.hidden = resolution.value !== "manual";
      if (!manual.hidden) {
        const d = await api("files", "transfer_preview", {
          ...context,
          path: item.path,
        });
        manual.value = d.new || "";
      }
    };
    row.append(c.label);
    if (item.conflict)
      row.append(
        el("p", "help", tr("Ana proje de değişmiş; çakışmayı çöz.")),
        resolution,
        manual,
      );
    row.append(
      button(
        tr("Farkı incele"),
        async () => {
          const d = await api("files", "transfer_preview", {
            ...context,
            path: item.path,
          });
          const pre = el("pre", "code-preview", d.patch || tr("İkili dosya"));
          if (row.querySelector("pre")) row.querySelector("pre").remove();
          else row.append(pre);
        },
        "quiet",
      ),
    );
    box.append(row);
    choices.push({ item, c: c.input, resolution, manual });
  }
  if (!items.length)
    box.append(el("p", "empty", tr("Aktarılacak değişiklik yok.")));
  modal(
    tr("Ana projeye aktar"),
    box,
    async () => {
      lastTransfer = await api("files", "transfer", {
        ...context,
        files: choices
          .filter((x) => x.c.checked)
          .map((x) => ({
            ...x.item,
            resolution: x.resolution.value,
            text: x.manual.value,
          })),
      });
      $("#transfer-undo").disabled = false;
      toast(
        tr("Seçilen değişiklikler ana projeye aktarıldı. Oturum çalışma alanı korundu."),
      );
    },
    tr("Seçilenleri aktar"),
  );
}
$("#transfer-open").onclick = () => transferDialog().catch(error);
$("#transfer-undo").onclick = async () => {
  try {
    await api("files", "undo_transfer", { ...ctx(), token: lastTransfer.id });
    $("#transfer-undo").disabled = true;
    toast(tr("Aktarım geri alındı"));
  } catch (e) {
    error(e);
  }
};

async function loadGitReview(context, state, token) {
  $("#git-branch").textContent = "⑂ " + state.branch;
  $("#git-target").textContent =
    state.upstream || tr("Push hedefi: origin / ") + (state.branch || "");
  $("#git-staged").replaceChildren();
  $("#git-working").replaceChildren();
  const entries = [], workingPaths = new Set();
  let staged = 0,
    working = 0;
  for (const file of state.files) {
    for (const isStaged of [true, false]) {
      const include = isStaged
        ? file.index !== " " && file.index !== "?"
        : file.work !== " " || file.untracked;
      if (!include) continue;
      if (isStaged) staged++;
      else {
        working++;
        if (!file.conflict) workingPaths.add(file.path);
      }
      const item = {
        ...file,
        status: file.conflict ? "!" : file.untracked ? "U" : isStaged ? file.index : file.work,
        stats: isStaged ? file.indexStats : file.workStats,
        staged: isStaged,
      };
      const comparison = tr(isStaged ? "Son commit → Hazırlanan içerik"
        : file.untracked ? "Yeni dosya → Çalışma alanı" : "Hazırlanan içerik → Çalışma alanı");
      const open = (row) => file.conflict ? resolveConflict(file, context, state.stamp, token)
        : showReview({ kind: "git", path: file.path, staged: isStaged },
          row, comparison, () => api("git", "diff", {
            ...context, path: file.path, staged: isStaged,
            oldPath: (isStaged ? file.index : file.work) === "R" ? file.oldPath : undefined,
          }), token);
      const row = reviewFileRow(item, open);
      if (!isStaged && !file.conflict) {
        const box = el("input");
        box.type = "checkbox";
        box.setAttribute("aria-label", tr("Hazırlamak için seç") + " " + file.path);
        box.checked = gitSelected.has(file.path);
        box.onchange = () => {
          if (box.checked) gitSelected.add(file.path);
          else gitSelected.delete(file.path);
          $("#stage-selected").disabled = !gitSelected.size;
        };
        row.prepend(box);
      }
      if (!file.conflict) row.append(
        button(
          isStaged ? "−" : "+",
          () => stage([file.path], isStaged),
          "icon",
          isStaged ? tr("Hazırlananlardan çıkar") : tr("Commit için hazırla"),
        ),
      );
      entries.push({ item, row, open });
      (isStaged ? $("#git-staged") : $("#git-working")).append(row);
    }
  }
  $("#staged-count").textContent = staged;
  $("#working-count").textContent = working;
  for (const path of gitSelected)
    if (!workingPaths.has(path)) gitSelected.delete(path);
  $("#stage-selected").disabled = !gitSelected.size;
  $("#stage-all").disabled = !working;
  $("#unstage-all").disabled = !staged;
  $("#git-commit").disabled = !staged;
  if (!staged)
    $("#git-staged").append(el("p", "help", tr("Henüz hazırlanmış dosya yok.")));
  if (!working) $("#git-working").append(el("p", "help", tr("Değişiklik yok.")));
  await openReviewEntry(entries, "git");
}
async function stage(files, unstage = false) {
  if (!gitState?.available || !files.length) return;
  const context = ctx();
  const sameContext = () => projectId === context.projectId && (focusId || undefined) === context.sessionId;
  await api("git", "stage", {
    ...context,
    stamp: gitState.stamp,
    files,
    unstage,
  });
  if (!sameContext()) return;
  gitSelected.clear();
  await refreshChanges();
  if (sameContext() && !unstage && files.length > 1 && $("#suggest-after-stage").checked)
    await suggestCommit(context.sessionId);
}
$("#stage-all").onclick = () =>
  stage(
    gitState.files
      .filter((f) => f.work !== " " || f.untracked)
      .map((f) => f.path),
  ).catch(error);
$("#stage-selected").onclick = () => stage([...gitSelected]).catch(error);
$("#unstage-all").onclick = () =>
  stage(
    gitState.files
      .filter((f) => f.index !== " " && f.index !== "?")
      .map((f) => f.path),
    true,
  ).catch(error);
$("#git-commit").onclick = busy($("#git-commit"), async () => {
  const scope = changesScope;
  const result = await api("git", "commit", {
    ...ctx(),
    stamp: gitState.stamp,
    message: $("#commit-message").value,
  });
  if (scope === changesScope) $("#commit-message").value = "";
  toast(result.output || tr("Commit oluşturuldu"));
  await refreshChanges();
});
$("#git-push").onclick = busy($("#git-push"), async () => {
  const result = await api("git", "push", {
    ...ctx(),
    stamp: gitState.stamp,
    remote: gitState.remotes.includes("origin")
      ? "origin"
      : gitState.remotes[0],
  });
  toast(result.output || tr("Push tamamlandı"));
  await refreshChanges();
});
$("#git-fetch").onclick = busy($("#git-fetch"), async () => {
  await api("git", "fetch", {
    ...ctx(),
    stamp: gitState.stamp,
    remote: gitState.remotes.includes("origin")
      ? "origin"
      : gitState.remotes[0],
  });
  toast(tr("Uzak depo güncellendi"));
  await refreshChanges();
});
async function suggestCommit(id = focusId) {
  if (!id || !active(id))
    throw Error(tr("Mesaj önerisi için seçili oturumu başlat"));
  const v = views.get(id);
  if (v.inputDraft || ["working", "permission"].includes(status(id).key))
    throw Error(tr("Ajan hazır olmalı ve giriş taslağı boş olmalı"));
  const message =
    "Git indexine hazırlanmış değişiklikleri incele. Yalnızca uygun bir commit mesajı öner. Commit veya push yapma; dosyaları değiştirme.";
  await pasteDraft(id, message);
  toast(
    tr("İstek terminale taslak olarak eklendi. Gönderdikten sonra önerilen mesajı commit alanına yazabilirsin."),
  );
}
$("#git-suggest").onclick = () => suggestCommit().catch(error);
$("#suggest-after-stage").checked =
  localStorage.getItem("agentloom-suggest") === "true";
$("#suggest-after-stage").onchange = (e) =>
  localStorage.setItem("agentloom-suggest", String(e.target.checked));
async function mergeTargetDialog() {
  const context = ctx();
  const catalog = await api("git", "merge_targets", context);
  if (context.projectId !== projectId || context.sessionId !== (focusId || undefined)) return;
  const target = select(catalog.targets.map(name => ({ id: name, label: name })));
  if (catalog.selected) target.value = catalog.selected;
  const direction = el("strong", "merge-direction");
  const status = el("p", "help");
  status.setAttribute("role", "status");
  const files = el("details", "merge-source-files");
  const recovery = el("div", "merge-recovery");
  const box = el("div", "merge-target-form");
  box.append(direction, field(tr("Hedef dal"), target), status, files, recovery,
    el("p", "help", tr("Kaynak oturum aynı dalda kalır. Birleştirme yalnızca seçilen yerel hedef dalı günceller.")));
  let preview = null, revision = 0, applying = false;
  let submit;
  modal(tr("Hedef dala birleştir"), box, async () => {
    if (!preview || applying) throw Error(tr("Önce geçerli bir hedef dal seç."));
    applying = true; target.disabled = true;
    try {
      const result = await api("git", "merge_target_apply", { ...context, target: preview.target, stamp: preview.stamp });
      if (!result.merged) {
        preview = null;
        files.open = true;
        files.replaceChildren(el("summary", "", tr("Çakışan dosyalar")), ...result.conflicts.map(name => el("code", "merge-file", name)));
        const recover = button(tr("Çakışmaları bu oturumda çöz"), () => {});
        recover.onclick = busy(recover, async () => {
          applying = true; target.disabled = true;
          try {
            const resolution = await api("git", "merge_target_resolve", {
              ...context, target: result.target, stamp: result.stamp,
            });
            if (box.isConnected && $("#dialog").open) closeModal();
            if (context.projectId === projectId && context.sessionId === focusId)
              await setChangesMode("git");
            toast(tr(resolution.conflicts.length
              ? "Dosyaları bu oturumda çözerek birleştirmeyi tamamla. Sonra hedef dala birleştirmeyi yeniden dene."
              : "Bu oturumdaki birleştirme sonucunu kontrol et. Ardından hedef dala birleştirmeyi yeniden dene."));
          } finally { applying = false; target.disabled = false; }
        });
        recovery.replaceChildren(recover);
        throw Error(tr("Çakışma bulundu; hedef dal değiştirilmedi. Çakışmaları bu oturumda çözebilirsin."));
      }
      toast(tr("{source} → {target} birleştirildi. Kaynak oturum korundu.", { source: catalog.source, target: result.target }));
      await refreshChanges();
    } catch (e) {
      // A failed/stale application must be reviewed again before another attempt.
      preview = null;
      throw e;
    } finally {
      applying = false; target.disabled = false;
      // modal()'s busy wrapper re-enables submit; keep it blocked until a fresh preview.
      setTimeout(() => { if (submit?.isConnected && !preview) submit.disabled = true; }, 0);
    }
  }, tr("Birleştir"));
  submit = $("#dialog-footer .primary");
  const load = async () => {
    if (applying) return;
    const current = ++revision;
    preview = null; submit.disabled = true;
    status.removeAttribute("title");
    recovery.replaceChildren(); files.replaceChildren();
    box.querySelectorAll(".dialog-error").forEach(node => node.remove());
    $("#dialog-body > .dialog-error")?.remove();
    direction.textContent = catalog.source + " → " + target.value;
    if (!target.value) { status.textContent = tr("Birleştirilecek başka bir yerel dal yok."); return; }
    status.textContent = tr("Birleştirme kontrol ediliyor…");
    try {
      const result = await api("git", "merge_target_preview", { ...context, target: target.value });
      if (current !== revision || !box.isConnected) return;
      if (result.source !== catalog.source) throw Error(tr("Kaynak dal değişti. Birleştirme penceresini yeniden aç."));
      status.textContent = result.unchanged ? tr("Kaynak değişiklikler hedef dalda zaten var.")
        : tr("Kaynak commit: {commits} · Değişen dosya: {files}", { commits: result.commits, files: result.files.length });
      if (result.directory) { status.title = result.directory; }
      if (result.files.length) files.append(el("summary", "", tr("Kaynak dalın değişiklikleri")), ...result.files.map(name => el("code", "merge-file", name)));
      preview = result; submit.disabled = result.unchanged;
    } catch (e) { if (current === revision && box.isConnected) status.textContent = uiError(e); }
  };
  target.onchange = load;
  const refresh = button(tr("Yeniden kontrol et"), load, "quiet");
  box.insertBefore(refresh, files);
  await load();
}

$("#git-more").onclick = () => {
  if (!gitState?.available) return;
  const box = el("div");
  for (const [id, label] of [
    ["branch", tr("Yeni dal")],
    ["switch", tr("Dal değiştir")],
    ["merge_target", tr("Hedef dala birleştir")],
    ["merge", tr("Bu dala başka dalı al")],
    ["pr", tr("Taslak PR oluştur")],
    ["merge_continue", tr("Birleştirmeyi tamamla")],
    ["merge_abort", tr("Birleştirmeyi iptal et")],
  ])
    box.append(button(label, () => id === "merge_target" ? mergeTargetDialog() : gitForm(id)));
  modal(tr("Git işlemleri"), box);
};
function gitForm(action) {
  if (!gitState?.available) return;
  const context = ctx(), stamp = gitState.stamp;
  const choices = select(
    (gitState.branches || []).map((b) => ({ id: b.name, label: b.name })),
  );
  const text = input(),
    body = el("textarea");
  let content,
    request = {};
  if (action === "branch") content = field(tr("Yeni dal adı"), text);
  else if (action === "pr") {
    text.placeholder = tr("PR başlığı");
    body.placeholder = tr("Açıklama");
    content = el("div");
    content.append(
      field(tr("Başlık"), text),
      field(tr("Hedef dal"), choices),
      field(tr("Açıklama"), body),
    );
  } else if (["merge_continue", "merge_abort"].includes(action))
    content = el(
      "p",
      "help",
      action === "merge_abort"
        ? tr("Devam eden Git birleştirmesi iptal edilecek.")
        : tr("Çözümlenmiş dosyalarla birleştirme tamamlanacak."),
    );
  else
    content = field(
      action === "merge" ? tr("Birleştirilecek dal") : tr("Dal"),
      choices,
    );
  modal(
    "Git · " + action,
    content,
    async () => {
      request =
        action === "branch"
          ? { name: text.value }
          : action === "pr"
            ? { title: text.value, body: body.value, base: choices.value }
            : action === "switch"
              ? { name: choices.value }
              : { target: choices.value };
      const r = await api("git", action, {
        ...context,
        stamp,
        ...request,
      });
      toast(r.output || tr("İşlem tamamlandı"));
      await refreshChanges();
    },
    tr("Uygula"),
  );
}
async function resolveConflict(file, context, stamp, token) {
  const revision = ++changeRevision;
  const data = await api("git", "conflict_details", { ...context, path: file.path });
  if (token !== panelGeneration || revision !== changeRevision || $("#dialog").open) return;
  const choice = select([
      ...(data.text === null ? [] : [{ id: "manual", label: tr("Elle düzenle") }]),
      { id: "ours", label: tr("Bizim sürümümüz") },
      { id: "theirs", label: tr("Gelen sürüm") },
    ]),
    editor = el("textarea");
  editor.value = data.text ?? "";
  editor.style.cssText = "width:100%;height:350px;font-family:Consolas";
  choice.onchange = () => { editor.hidden = choice.value !== "manual"; };
  choice.onchange();
  modal(
    tr("Çakışmayı çöz · ") + file.path,
    [choice, editor],
    async () => {
      await api("git", "conflict_resolve", {
        ...context,
        stamp,
        path: file.path,
        choice: choice.value,
        text: editor.value,
        fileStamp: data.hash,
      });
      await refreshChanges();
    },
    tr("Çöz ve hazırla"),
  );
}
async function loadHistory(context = ctx(), token = panelGeneration) {
  const revision = ++historyRevision;
  clearReview();
  $("#history-files").replaceChildren();
  const root = $("#git-history");
  root.replaceChildren(el("p", "help", tr("Yükleniyor…")));
  const from = $("#history-from"), to = $("#history-to"), swap = $("#history-swap");
  from.disabled = to.disabled = swap.disabled = true;
  const history = await api("git", "history", {
    ...context,
    branch: $("#history-branch").value,
  });
  if (token !== panelGeneration || revision !== historyRevision) return;
  root.replaceChildren();
  const hashes = new Set(history.map((commit) => commit.hash));
  if (!hashes.has(selectedCommit)) selectedCommit = history[0]?.hash ?? null;
  if (!hashes.has(baseCommit) || baseCommit === selectedCommit) baseCommit = null;
  const options = (control, first) => {
    control.replaceChildren(...(first ? [first] : []),
      ...history.map((commit) =>
        new Option(commit.hash.slice(0, 7) + " · " + commit.subject, commit.hash)));
    control.disabled = !history.length;
  };
  options(from, new Option(tr("Önceki commit"), ""));
  options(to);
  const selectCommits = async (selected, base = null) => {
    if (token !== panelGeneration || revision !== historyRevision) return;
    selectedCommit = selected;
    baseCommit = base === selected ? null : base;
    from.value = baseCommit || "";
    to.value = selectedCommit || "";
    swap.disabled = !baseCommit;
    for (const row of root.children) {
      row.classList.toggle("active", row.dataset.hash === selectedCommit);
      row.classList.toggle("base", row.dataset.hash === baseCommit);
    }
    const parent = history.find((commit) => commit.hash === selectedCommit)?.parents?.[0];
    if (selectedCommit) await showCommitReview(context, token, parent);
  };
  to.onchange = () => selectCommits(to.value, from.value).catch(error);
  from.onchange = () => selectCommits(to.value, from.value).catch(error);
  swap.onclick = () => selectCommits(from.value, to.value).catch(error);
  let lanes = [];
  const colors = ["#a6d47e", "#a4bbee", "#d2a0de", "#e4bd78", "#77c6bc"];
  for (const commit of history) {
    const previous = [...lanes];
    let lane = lanes.indexOf(commit.hash);
    if (lane < 0) {
      lane = lanes.length;
      lanes.push(commit.hash);
    }
    const pointLane = lane;
    lanes.splice(lane, 1);
    const parents = commit.parents || [];
    for (let i = 0; i < parents.length; i++)
      if (!lanes.includes(parents[i]))
        lanes.splice(Math.min(pointLane + i, lanes.length), 0, parents[i]);
    const row = button("", (e) => {
      $("#git-history-picker").open = false;
      return e.ctrlKey && selectedCommit && selectedCommit !== commit.hash
        ? selectCommits(selectedCommit, commit.hash) : selectCommits(commit.hash);
    }, "history-row");
    row.dataset.hash = commit.hash;
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute(
      "width",
      Math.max(32, (Math.max(previous.length, lanes.length) + 1) * 14),
    );
    svg.setAttribute("height", "38");
    const line = (x1, y1, x2, y2, color) => {
      const n = document.createElementNS(svg.namespaceURI, "path");
      n.setAttribute("d", `M${x1} ${y1} L${x2} ${y2}`);
      n.setAttribute("stroke", color);
      n.setAttribute("stroke-width", "2");
      n.setAttribute("fill", "none");
      svg.append(n);
    };
    previous.forEach((h, i) => {
      if (h === commit.hash)
        line(
          10 + i * 14,
          0,
          10 + pointLane * 14,
          19,
          colors[i % colors.length],
        );
      else {
        const after = lanes.indexOf(h);
        if (after >= 0)
          line(10 + i * 14, 0, 10 + after * 14, 38, colors[i % colors.length]);
      }
    });
    for (const parent of parents) {
      const at = lanes.indexOf(parent);
      line(
        10 + pointLane * 14,
        19,
        10 + at * 14,
        38,
        colors[pointLane % colors.length],
      );
    }
    const dot = document.createElementNS(svg.namespaceURI, "circle");
    dot.setAttribute("cx", 10 + pointLane * 14);
    dot.setAttribute("cy", "19");
    dot.setAttribute("r", "4");
    dot.setAttribute("fill", colors[pointLane % colors.length]);
    svg.append(dot);
    const subject = el("span", "subject");
    if (commit.refs) subject.append(el("span", "ref-tag", commit.refs));
    subject.append(document.createTextNode(commit.subject || ""));
    row.append(svg, subject, el("small", "", commit.hash.slice(0, 7)));
    row.title = commit.subject + "\n" + commit.author + " · " + commit.date;
    root.append(row);
  }
  if (!history.length) root.append(el("p", "empty", tr("Commit geçmişi boş.")));
  await selectCommits(selectedCommit, baseCommit);
}
$("#history-branch").onchange = () => refreshChanges().catch(error);
async function showCommitReview(context, token, parent) {
  clearReview();
  const revision = changeRevision;
  const selected = selectedCommit, base = baseCommit;
  const args = { ...context, commit: selected, from: base };
  const list = $("#history-files");
  list.replaceChildren(el("p", "help", tr("Dosyalar karşılaştırılıyor…")));
  const rows = await api("git", "commit_files", args);
  if (token !== panelGeneration || revision !== changeRevision) return;
  const comparison = ((base || parent)?.slice(0, 8) || tr("Boş ağaç")) +
    " → " + selected.slice(0, 8);
  const entries = [];
  list.replaceChildren();
  for (const item of rows) {
    const open = (row) => showReview({ kind: "history", path: item.path }, row,
      comparison, () => api("git", "commit_diff", { ...args, path: item.path }), token);
    const row = reviewFileRow(item, open);
    list.append(row);
    entries.push({ item, row, open });
  }
  if (!rows.length) list.append(el("p", "empty", tr("Dosya farkı yok.")));
  await openReviewEntry(entries, "history");
}

async function openLink(source, target) {
  if (!source) {
    source = focusId;
  }
  const candidates = projectSessions().filter((s) => s.id !== source),
    choice = select(
      [
        { id: "", label: tr("Hedef oturum seç") },
        ...candidates.map((s) => ({
          id: s.id,
          label: s.name + " · " + names[s.provider],
        })),
      ],
      target,
    ),
    existing = db.links.find((l) => l.source === source && l.target === target),
    read = check(tr("Konuşma geçmişini okuyabilsin")),
    files = check(tr("Dosyalarını okuyabilsin")),
    work = check(tr("Bu oturumdan iş isteyebilsin")),
    status = el("p", "link-reapproval"),
    unlink = button(
      tr("Bağlantıyı kes"),
      async () => {
        await api("app", "unlink", { id: current.id });
        closeModal();
        await sync();
      },
      "danger",
    ),
    box = el("div");
  let current = existing;
  choice.disabled = Boolean(existing);
  status.textContent = tr("Bu bağlantıdaki bir oturumun konuşması değişti. İzinleri yeniden onayla.");
  box.append(
    el(
      "p",
      "help",
      (session(source)?.name || tr("Oturum")) +
        tr(" → seçilen oturum. İzin tek yönlüdür; dosyalar ortak hale gelmez."),
    ),
    field(tr("Hedef oturum"), choice),
    status,
    read.label,
    files.label,
    work.label,
    unlink,
  );
  modal(tr("Oturum bağlantısı"), box, async () => {
    await api("app", "link", {
      source,
      target: choice.value,
      read: read.input.checked,
      files: files.input.checked,
      work: work.input.checked,
    });
    await sync();
  });
  const save = $("#dialog-footer .primary");
  function updateTarget() {
    current = db.links.find((l) => l.source === source && l.target === choice.value);
    read.input.checked = current?.read ?? true;
    files.input.checked = current?.files ?? false;
    work.input.checked = current?.work ?? false;
    status.hidden = !current?.needsReapproval;
    unlink.hidden = !current;
    save.disabled = !choice.value;
    save.textContent = tr(current?.needsReapproval ? "Yeniden onayla" : "Kaydet");
  }
  choice.onchange = updateTarget;
  updateTarget();
}
function renderLinks() {
  const box = $("#links-list");
  box.replaceChildren();
  for (const l of db.links.filter(
    (l) => l.source === focusId || l.target === focusId,
  )) {
    const card = el("article", "link-card");
    card.append(
      el("h3", "", session(l.source)?.name + " → " + session(l.target)?.name),
      el(
        "p",
        "muted",
        [
          l.read ? tr("Konuşma okuma") : "",
          l.files ? tr("Dosya okuma") : "",
          l.work ? tr("İş isteme") : "",
        ]
          .filter(Boolean)
          .join(" · "),
      ),
    );
    if (l.needsReapproval) card.append(el("p", "link-reapproval", tr("Yeniden onay gerekli")));
    const row = el("div", "row");
    row.append(
      button(tr("İzinler"), () => openLink(l.source, l.target)),
      button(tr("Oturuma git"), () => selectSession(l.target, true)),
      button(
        tr("Bağlantıyı kes"),
        async () => {
          await api("app", "unlink", { id: l.id });
          await sync();
        },
        "quiet",
      ),
    );
    card.append(row);
    box.append(card);
  }
  if (!box.childNodes.length)
    box.append(el("p", "help", tr("Bu oturumun henüz bağlantısı yok.")));
  const jobs = $("#jobs-list");
  jobs.replaceChildren();
  for (const j of db.jobs.filter(
    (j) => j.source === focusId || j.target === focusId,
  )) {
    const card = el("article", "job-card");
    card.append(
      el(
        "strong",
        "",
        session(j.source)?.name + " → " + session(j.target)?.name,
      ),
      el("span", "badge", j.state),
      el("pre", "", j.instruction),
    );
    if (j.result) card.append(el("pre", "", j.result));
    if (j.state === "pending") {
      card.append(
        button(tr("Kabul et · taslağa ekle"), async () => {
          selectSession(j.target, true);
          const view = draftTarget(j.target);
          const runId = view.runId;
          view.term.options.disableStdin = true;
          try {
            const result = await api("app", "job", { id: j.id, decision: "accept", runId });
            if (view.runId === runId) view.inputDraft = result.text;
            view.term.focus();
            if (result.saved) {
              toast(tr("Görev taslağa eklendi. Göndermek için terminalde Enter’a bas."));
            } else {
              toast(tr("Taslak eklendi ancak istek durumu diske kaydedilemedi."), true);
            }
          } finally {
            view.term.options.disableStdin = false;
          }
          await sync();
        }),
        button(
          tr("İptal"),
          () => api("app", "job", { id: j.id, decision: "cancel" }),
          "quiet",
        ),
      );
    }
    jobs.append(card);
  }
}
$("#link-add").onclick = () => openLink(focusId).catch(error);

function paintBrowserPermission() {
  const enabled = session(focusId)?.browserAccess !== false;
  $("#browser-access").textContent = enabled
    ? tr("Ajan erişimi açık")
    : tr("Ajan erişimi kapalı");
  $("#browser-access").classList.toggle("selected", enabled);
}
function paintPreview(id, view) {
  const state = previews[id]?.state;
  view.preview.hidden = session(id)?.login === true;
  view.preview.textContent =
    { starting: tr("◌ Önizleme"), ready: tr("● Önizleme"), error: tr("! Önizleme") }[
      state
    ] || tr("▶ Önizleme");
  view.preview.title =
    {
      starting: tr("Sunucu bekleniyor · Ayrıntıları göster"),
      ready: tr("Önizleme hazır · Ayrıntıları göster"),
      error: tr("Önizleme hatası · Ayrıntıları göster"),
    }[state] || tr("Bu oturumun uygulamasını tarayıcıda önizle");
  view.preview.classList.toggle("selected", state === "ready");
  view.preview.classList.toggle("danger", state === "error");
}

async function startPreview(id, settings) {
  const run = await api("preview", "start", { sessionId: id, settings });
  previews[id] = run;
  const view = views.get(id);
  if (view) paintPreview(id, view);
}

async function usePreview(id) {
  const s = session(id);
  const run = await api("preview", "status", { sessionId: id });
  if (!s) return;
  if (run?.state === "starting" || run?.state === "ready") {
    const info = el("p", "help", tMessage(run.message) + "\n" + run.url);
    const box = el("div");
    const output = el("pre", "preview-output", run.log);
    const show = button(tr("Tarayıcıda göster"), async () => {
      await api("preview", "open", { sessionId: id });
      closeModal();
      selectSession(id);
      await openPanel("browser");
    });
    show.disabled = run.state !== "ready";
    const stop = button(
      tr("Önizlemeyi durdur"),
      async () => {
        await api("preview", "stop", { sessionId: id, runId: run.runId });
        closeModal();
      },
      "danger",
    );
    box.append(info, el("div", "row"), output);
    box.children[1].append(show, stop);
    modal(s.name + tr(" · Önizleme"), box);
    const refresh = async () => {
      const current = await api("preview", "status", { sessionId: id });
      if (!box.isConnected || !$("#dialog").open) return;
      info.textContent = tMessage(current.message) + "\n" + current.url;
      output.textContent = current.log;
      show.disabled = current.state !== "ready";
      stop.disabled = !["starting", "ready"].includes(current.state);
    };
    const timer = setInterval(() => refresh().catch(error), 750);
    $("#dialog").addEventListener("close", () => clearInterval(timer), {
      once: true,
    });
    return;
  }
  if (s.preview && (!run || run.state === "stopped")) {
    try {
      await startPreview(id, undefined);
      return;
    } catch (e) {
      previewSettings(s, uiError(e));
      return;
    }
  }
  previewSettings(s, run?.state === "error" ? tMessage(run.message) : "", run?.log);
}

function previewSettings(s, message = "", output = "") {
  const command = input(
    s.preview?.command,
    tr("Örn. dotnet run --urls http://127.0.0.1:5080"),
  );
  const address = input(s.preview?.url, "http://127.0.0.1:5080/");
  const fields = [
    el(
      "p",
      "form-note",
      tr("Komut bu oturumun çalışma alanında PowerShell ile çalışır. Adres ve port, komutun açtığı yerel sunucuyla aynı olmalı."),
    ),
    field(tr("Önizleme komutu"), command),
    field(tr("Yerel adres"), address),
  ];
  if (message) fields.push(el("p", "dialog-error", message));
  if (output) fields.push(el("pre", "preview-output", output));
  modal(
    s.name + tr(" · Önizleme"),
    fields,
    () => startPreview(s.id, { command: command.value, url: address.value }),
    tr("Önizlemeyi başlat"),
  );
}

let traceDialog = null;
const traceLabels = {
  open: tr("Sayfa açma"),
  click: tr("Tıklama"),
  double_click: tr("Çift tıklama"),
  right_click: tr("Sağ tıklama"),
  hover: tr("Üzerine gelme"),
  fill: tr("Alan doldurma"),
  type: tr("Metin yazma"),
  select: tr("Seçenek seçme"),
  check: tr("Onay kutusu"),
  key: tr("Tuşa basma"),
  scroll: tr("Kaydırma"),
  drag: tr("Sürükleme"),
  wait: tr("Bekleme"),
  text_contains: tr("Metin kontrolü"),
  text_absent: tr("Metnin yokluğunu kontrol et"),
  url_contains: tr("Adres kontrolü"),
};

async function renderTrace(dialog) {
  const revision = ++dialog.revision;
  const steps = await api("browser", "trace", { sessionId: dialog.sid });
  if (
    traceDialog !== dialog ||
    revision !== dialog.revision ||
    !dialog.list.isConnected
  )
    return;
  dialog.clear.disabled = steps.length === 0;
  dialog.list.replaceChildren(...traceRows(dialog.sid, steps));
  dialog.empty.hidden = steps.length > 0;
}
function traceRows(sid, steps) {
  return steps.map((step) => {
    const row = el("li", "trace-step");
    const title = el(
      "strong",
      "",
      traceLabels[step.operation] || tr("Tarayıcı işlemi"),
    );
    if (step.detail) title.textContent += " · " + step.detail;
    const status = el(
      "span",
      "trace-state",
      step.passed
        ? step.kind === "assert"
          ? tr("✓ Doğrulandı")
          : tr("✓ Tamamlandı")
        : tr("× Başarısız"),
    );
    status.classList.toggle("failed", !step.passed);
    const header = el("div", "row");
    header.append(title, status);
    row.append(
      header,
      el(
        "small",
        "",
        [
          new Date(step.time).toLocaleString(uiLocale),
          step.durationMs + " ms",
          step.url,
        ]
          .filter(Boolean)
          .join(" · "),
      ),
    );
    if (step.hasImage) {
      const imageButton = button(
        tr("Hata ekranını göster"),
        async () => {
          const data = await api("browser", "trace_image", {
            sessionId: sid,
            stepId: step.id,
          });
          if (!data || !row.isConnected) return;
          const image = el("img", "trace-image");
          image.alt = tr("Başarısız tarayıcı adımının ekran görüntüsü");
          image.src = data;
          imageButton.replaceWith(image);
        },
        "quiet",
      );
      row.append(imageButton);
    }
    return row;
  });
}
async function loadSummaryTrace(sid, token) {
  const revision = ++summaryTraceRevision;
  const current = () =>
    token === panelGeneration && revision === summaryTraceRevision;
  const count = $("#summary-trace-count");
  try {
    const { steps, passed, failed, total } = await api(
      "browser",
      "trace_summary",
      { sessionId: sid },
    );
    if (!current()) return;
    const checks = steps.filter((step) => step.kind === "assert");
    const verification = passed + failed
      ? tr("{passed} başarılı · {failed} başarısız", { passed, failed })
      : tr("Henüz doğrulama yok");
    count.textContent = total
      ? verification + " · " + countLabel(total, "{count} tarayıcı adımı") +
        (total > steps.length ? countLabel(steps.length, " · son {count} adım listeleniyor") : "")
      : tr("Henüz kayıtlı tarayıcı adımı yok.");
    $("#summary-trace").replaceChildren(...traceRows(sid, checks.reverse()));
    $("#summary-all-trace").disabled = steps.length === 0;
  } catch (e) {
    if (current()) count.textContent = tr("Tarayıcı adımları alınamadı. ") + uiError(e);
  }
}
async function openTrace(sid) {
  if (!sid) return;
  const box = el("div");
  const list = el("ol", "trace-list");
  const empty = el(
    "p",
    "help",
    tr("Bu oturumda henüz ajan tarafından kaydedilmiş test adımı yok."),
  );
  const clear = button(
    tr("Adımları temizle"),
    async () => {
      await api("browser", "trace_clear", { sessionId: sid });
      await renderTrace(dialog);
    },
    "quiet",
  );
  const dialog = { sid, list, empty, clear, revision: 0 };
  box.append(clear, empty, list);
  modal((session(sid)?.name || tr("Tarayıcı")) + tr(" · Test adımları"), box);
  traceDialog = dialog;
  $("#dialog").addEventListener(
    "close",
    () => {
      if (traceDialog === dialog) traceDialog = null;
    },
    { once: true },
  );
  await renderTrace(dialog);
}
$("#browser-trace").onclick = () => openTrace(focusId).catch(error);

async function loadBrowser() {
  $("#browser-trace").disabled = !focusId;
  if (!focusId) {
    browserPage = null;
    browserTabs = [];
    paintBrowserTabs();
    hideBrowser();
    return;
  }
  const sid = focusId,
    result = await api("browser", "tabs", { sessionId: sid });
  if (sid !== focusId) return;
  browserTabs = result.tabs;
  browserPage =
    browserTabs.find((t) => t.id === result.active) || browserTabs[0] || null;
  paintBrowserTabs();
  if (browserPage) {
    $("#address").value = browserPage.url;
    $("#browser-mode").value = browserPage.mode;
    paintPick();
    scheduleViewport();
  } else {
    $("#address").value = "";
    $("#picked").hidden = true;
    hideBrowser();
    $("#browser-surface").replaceChildren(
      el("div", "empty", tr("Bir web adresi aç.")),
    );
  }
}
function paintBrowserTabs() {
  const box = $("#browser-tabs");
  box.replaceChildren();
  for (const t of browserTabs) {
    const b = button(
      "",
      async () => {
        await api("browser", "activate", { sessionId: focusId, pageId: t.id });
        jsonVisible = false;
        await loadBrowser();
      },
      browserPage?.id === t.id ? "selected" : "",
    );
    const label = el("span", "label", t.title || t.url);
    label.title = t.url;
    const close = el("span", "close", "×");
    close.title = tr("Sekmeyi kapat");
    close.onclick = async (e) => {
      e.stopPropagation();
      try {
        await api("browser", "close", { sessionId: t.sessionId, pageId: t.id });
        await loadBrowser();
      } catch (e) {
        error(e);
      }
    };
    b.append(label, close);
    box.append(b);
  }
  const add = button(
    "+",
    () => {
      browserPage = null;
      jsonVisible = false;
      paintBrowserTabs();
      $("#address").value = "";
      $("#address").focus();
      hideBrowser();
      $("#browser-surface").replaceChildren(
        el("div", "empty", tr("Yeni sekme için bir adres gir.")),
      );
    },
    "icon quiet",
    tr("Yeni sekme"),
  );
  add.style.marginLeft = "auto";
  box.append(add);
}
async function openAddress(forceMode) {
  if (!focusId) throw Error(tr("Önce bir oturum seç"));
  const sid = focusId,
    mode = forceMode || $("#browser-mode").value;
  $("#browser-notice").textContent = tr("Sayfa açılıyor…");
  const p = await api("browser", "open", {
    sessionId: sid,
    pageId: browserPage?.id,
    url: $("#address").value,
    mode,
    newTab: !browserPage || browserPage.mode !== mode,
  });
  if (sid !== focusId) return;
  browserPage = p;
  jsonVisible = false;
  browserJson = null;
  await loadBrowser();
  setTimeout(() => pollBrowser().catch(() => {}), 700);
}
$("#browser-open").onclick = busy($("#browser-open"), () => openAddress());
$("#address").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("#browser-open").click();
});
$("#browser-chrome").onclick = busy($("#browser-chrome"), () =>
  openAddress("chrome"),
);
$("#browser-session").onchange = (e) => selectSession(e.target.value);
$("#browser-mode").onchange = () => {
  if (browserPage && $("#address").value) openAddress().catch(error);
};
$("#browser-access").onclick = async () => {
  if (!focusId) return;
  try {
    await api("app", "session_patch", {
      id: focusId,
      browserAccess: session(focusId).browserAccess === false,
    });
    await sync();
  } catch (e) {
    error(e);
  }
};
for (const a of ["back", "forward", "reload"])
  $("#browser-" + a).onclick = async () => {
    try {
      if (!browserPage) return;
      await api("browser", a, { sessionId: focusId, pageId: browserPage.id });
      setTimeout(() => pollBrowser().catch(() => {}), 350);
    } catch (e) {
      error(e);
    }
  };
let pickBusy = false;
async function setPicker(cancel = false) {
  const p = browserPage;
  if (!p) throw Error(tr("Önce bir sayfa aç"));
  if (pickBusy) return;
  pickBusy = true;
  $("#browser-pick").disabled = true;
  try {
    if (!cancel && !p.picking) {
      jsonVisible = false;
      scheduleViewport();
      await new Promise((r) => setTimeout(r, 100));
    }
    const result = await api(
      "browser",
      cancel || p.picking ? "cancel_pick" : "pick",
      { sessionId: focusId, pageId: p.id },
    );
    if (browserPage?.id === p.id) browserPage = result;
  } finally {
    pickBusy = false;
    $("#browser-pick").disabled = false;
    paintPick();
  }
}
$("#browser-pick").onclick = () => setPicker().catch(error);
function paintPick() {
  const picking = browserPage?.picking === true;
  $("#browser-pick").textContent = picking ? tr("× Seçimi iptal et") : tr("✧ Öğe seç");
  $("#browser-pick").setAttribute("aria-pressed", String(picking));
  $("#browser-pick").classList.toggle("selected", picking);
  const p = browserPage?.picked;
  $("#picked").hidden = !p;
  if (p) $("#picked-label").textContent = p.selector || tr("Seçili öğe");
  $("#browser-notice").textContent = browserPage?.picking
    ? tr("Sayfada bir öğeye tıkla. İptal: Esc veya Seçimi iptal et.")
    : browserPage?.mode === "chrome"
      ? tr("Chrome penceresi bağlı.")
      : tr("Önizleme hazır.");
}
$("#clear-pick").onclick = busy($("#clear-pick"), async () => {
  const p = browserPage;
  if (!p?.picked) return;
  const result = await api("browser", "clear_pick", {
    sessionId: focusId,
    pageId: p.id,
    pickId: p.picked.id,
  });
  if (browserPage?.id === p.id) {
    browserPage = result;
    paintPick();
  }
});
function draftTarget(id) {
  const v = views.get(id);
  if (!active(id) || !v?.ready || !v.runId)
    throw Error(tr("Bu oturumun terminalini önce başlat"));
  if (
    v.hasPendingInput() || v.inputDraft.trim() ||
    v.term.options.disableStdin ||
    ["working", "permission"].includes(status(id).key)
  )
    throw Error(tr("Ajanın hazır olmasını bekle ve giriş taslağını temizle"));
  return v;
}
async function pasteDraft(id, text) {
  const v = draftTarget(id);
  await api("terminal", "write", {
    id,
    runId: v.runId,
    data: "\x1b[200~" + text.replace(/[\x00-\x1f\x7f]/g, " ") + "\x1b[201~",
  });
  v.inputDraft = text;
  v.term.focus();
}
$("#attach-pick").onclick = busy($("#attach-pick"), async () => {
  if (!browserPage?.picked) return;
  const context = await api("browser", "attachment", {
    sessionId: focusId,
    pageId: browserPage.id,
    pickId: browserPage.picked.id,
  });
  await pasteDraft(
    focusId,
    tr(
      "{url} sayfasındaki {selector} öğesini incele. HTML ve stil bağlamı {contextFile} dosyasında, ekran görüntüsü ise {screenshot} dosyasında.",
      context,
    ),
  );
  toast(tr("Seçim terminale taslak olarak eklendi. Göndermek için Enter’a bas."));
});
let browserPollBusy = false,
  viewportTimer,
  inspectedDocument = null;
async function pollBrowser() {
  if (
    browserPollBusy ||
    !browserPage ||
    panel !== "browser" ||
    $("dialog[open]") ||
    document.body.classList.contains("no-right")
  )
    return;
  browserPollBusy = true;
  const sid = focusId,
    pid = browserPage.id;
  try {
    const p = await api("browser", "poll", { sessionId: sid, pageId: pid });
    if (focusId !== sid || browserPage?.id !== pid) return;
    const changed = browserPage.url !== p.url;
    browserPage = p;
    const at = browserTabs.findIndex((t) => t.id === pid);
    if (at >= 0) browserTabs[at] = p;
    paintPick();
    if (document.activeElement !== $("#address")) $("#address").value = p.url;
    if (changed) paintBrowserTabs();
    if (
      !p.picking &&
      !p.picked &&
      (inspectedDocument !== p.id + p.url || changed)
    ) {
      const data = await api("browser", "response", {
        sessionId: sid,
        pageId: pid,
      });
      if (focusId !== sid || browserPage?.id !== pid) return;
      inspectedDocument = p.id + p.url;
      if (data.json !== null && data.json !== undefined) {
        browserJson = { value: data.json, raw: data.text };
        jsonVisible = true;
        paintJson();
        scheduleViewport();
      } else if (changed) {
        browserJson = null;
        jsonVisible = false;
        paintJson();
        scheduleViewport();
      }
    }
  } finally {
    browserPollBusy = false;
  }
}
function paintJson() {
  $("#json-toolbar").hidden = !browserJson;
  $("#json-view").hidden = !browserJson || !jsonVisible;
  $("#browser-surface").hidden = !!browserJson && jsonVisible;
  $("#json-pretty").classList.toggle("selected", jsonPretty);
  $("#json-raw").classList.toggle("selected", !jsonPretty);
  if (!browserJson) return;
  const text = jsonPretty
    ? JSON.stringify(browserJson.value, null, 2)
    : browserJson.raw;
  const target = $("#json-view");
  target.replaceChildren();
  if (!jsonPretty) {
    target.textContent = text;
    return;
  }
  const regex =
    /("(?:\\u[0-9a-fA-F]{4}|\\[^u]|[^\\"])*"\s*:?)|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|\b(true|false|null)\b/g;
  let pos = 0;
  for (const m of text.matchAll(regex)) {
    target.append(document.createTextNode(text.slice(pos, m.index)));
    target.append(
      el(
        "span",
        m[1]
          ? m[0].endsWith(":")
            ? "json-key"
            : "json-string"
          : m[2]
            ? "json-number"
            : m[3] === "null"
              ? "json-null"
              : "json-bool",
        m[0],
      ),
    );
    pos = m.index + m[0].length;
  }
  target.append(document.createTextNode(text.slice(pos)));
}
$("#json-pretty").onclick = () => {
  jsonPretty = true;
  jsonVisible = true;
  paintJson();
  scheduleViewport();
};
$("#json-raw").onclick = () => {
  jsonPretty = false;
  jsonVisible = true;
  paintJson();
  scheduleViewport();
};
$("#json-copy").onclick = async () => {
  try {
    await navigator.clipboard.writeText(
      jsonPretty ? JSON.stringify(browserJson.value, null, 2) : browserJson.raw,
    );
    toast(tr("Cevap kopyalandı"));
  } catch (e) {
    error(e);
  }
};
$("#json-page").onclick = () => {
  jsonVisible = !jsonVisible;
  paintJson();
  scheduleViewport();
};
async function hideBrowser() {
  try {
    await api("browser", "hide");
  } catch {}
}
function scheduleViewport() {
  clearTimeout(viewportTimer);
  return new Promise((resolve) => {
    viewportTimer = setTimeout(async () => {
      try {
        paintJson();
        if (
          !browserPage ||
          panel !== "browser" ||
          $("dialog[open]") ||
          document.body.classList.contains("no-right") ||
          jsonVisible
        ) {
          await hideBrowser();
          return;
        }
        if (browserPage.mode === "chrome") {
          $("#browser-surface").replaceChildren(
            el(
              "div",
              "empty",
              tr("Sayfa bilgisayarındaki Chrome penceresinde açık. Öğe seçmek için “Öğe seç” düğmesine basıp Chrome’da öğeye tıkla."),
            ),
          );
          return;
        }
        const r = $("#browser-surface").getBoundingClientRect();
        await api("browser", "viewport", {
          sessionId: focusId,
          pageId: browserPage.id,
          x: r.x,
          y: r.y,
          width: r.width,
          height: r.height,
          visible: true,
        });
      } catch {
      } finally {
        resolve();
      }
    }, 50);
  });
}

async function initialize() {
  try {
    const saved = JSON.parse(
      localStorage.getItem("agentloom-layout") || "null",
    );
    if (saved) {
      ({ projectId, focusId, mode, camera } = Object.assign(
        { camera },
        saved,
      ));
      Object.assign(projectFocus, saved.projectFocus);
      Object.assign(projectFolders, saved.projectFolders);
      Object.assign(folderLayout, saved.folderLayout);
      if (Array.isArray(saved.collapsedSessionFolders))
        for (const key of saved.collapsedSessionFolders)
          if (typeof key === "string") collapsedSessionFolders.add(key);
      folderId = typeof saved.folderId === "string" ? saved.folderId : null;
      if (projectId && focusId) projectFocus[projectId] = focusId;
      panel = saved.panel || "browser";
      if (["git", "session"].includes(saved.changesPreference))
        changesPreference = saved.changesPreference;
      else if (saved.panel === "changes") changesPreference = "session";
      document.body.classList.toggle("no-left", !!saved.noLeft);
      document.body.classList.toggle("no-right", !!saved.noRight);
      $("#providers-section").hidden = !saved.showProviders;
    }
    const widths = JSON.parse(
      localStorage.getItem("agentloom-widths") || "null",
    );
    for (const [side, panel] of Object.entries(sidePanels)) {
      const width = widths?.[side]?.trim?.();
      if (/^\d+(\.\d+)?px$/.test(width))
        document.documentElement.style.setProperty(panel.variable, width);
    }
    const sections = JSON.parse(
      localStorage.getItem("agentloom-sections") || "{}",
    );
    for (const [id, height] of Object.entries(sections))
      if ($("#" + id)) $("#" + id).style.height = height;
  } catch {}
  await listen("notification-session", ({ payload }) =>
    selectSession(payload, true),
  );
  await listen("terminal", ({ payload }) => terminalPacket(payload));
  await listen("workflow-update", ({ payload }) => paintUpdate(payload));
  api("updates", "status").then(state => paintUpdate(state)).catch(() => {});
  await listen("browser-trace", ({ payload }) => {
    if (traceDialog?.sid === payload.sessionId && $("#dialog").open)
      renderTrace(traceDialog).catch(error);
    if (panel === "summary" && focusId === payload.sessionId)
      loadSummaryTrace(focusId, panelGeneration);
  });
  await listen("preview", async ({ payload }) => {
    try {
      const id = payload.sessionId;
      const current = await api("preview", "status", { sessionId: id });
      if (current?.runId !== payload.runId) return;
      previews[id] = current;
      const view = views.get(id);
      if (view) paintPreview(id, view);
      if (current.state === "error") toast(tMessage(current.message), true);
      if (current.state === "ready" && current.pageId && id === focusId)
        await openPanel("browser");
    } catch (e) {
      error(e);
    }
  });
  await listen("workspace", ({ payload }) => {
    const previous = session(focusId);
    db = payload;
    render();
    if (panel === "summary") {
      const current = session(focusId);
      if (previous?.id !== current?.id || previous?.login !== current?.login)
        refreshPanel();
      else if (current && !current.login)
        renderSummaryHeader(current);
    }
  });
  await listen("browser", ({ payload }) => {
    if (payload.sessionId === focusId && panel === "browser") {
      if (!browserPage || payload.id === browserPage.id) {
        browserPage = payload;
        paintPick();
      }
      if (!browserTabs.some((p) => p.id === payload.id))
        loadBrowser().catch(error);
    }
  });
  await sync();
  previews = await api("preview", "list").catch(() => ({}));
  await initializeNotifications();
  for (const [id, view] of views) paintPreview(id, view);
  await scanProviders();
  setMode(mode);
  await openPanel(panel, false);
  $("#show-providers").setAttribute("aria-expanded", String(!$("#providers-section").hidden));
  const connected = providers.some((p) => p.installed && p.accounts.some(isConnected));
  if (!connected && !db.sessions.length && !localStorage.getItem("workflow-setup-seen")) {
    await Promise.allSettled(providers.filter((p) => p.installed)
      .map((p) => refreshProviderUsage(p.id)));
    localStorage.setItem("workflow-setup-seen", "true");
    if (!providers.some((p) => p.installed && p.accounts.some(isConnected))) await openProviderSetup();
  }
  window.addEventListener("resize", () => {
    if (templateMenu.matches(":popover-open")) positionTemplateMenu();
    if (workspaceMenu.matches(":popover-open")) positionWorkspaceMenu();
    fitTerminals();
    scheduleViewport();
  });
  $("#project-sessions").addEventListener("scroll", () => {
    if (workspaceMenu.matches(":popover-open")) positionWorkspaceMenu();
  });
  new ResizeObserver(scheduleViewport).observe($("#browser-surface"));
  new ResizeObserver(() => {
    fitTerminals();
    scheduleViewport();
  }).observe($("#layout"));
  setInterval(
    () =>
      pollBrowser().catch((e) => {
        $("#browser-notice").textContent = uiError(e);
      }),
    850,
  );
  window.addEventListener("beforeunload", remember);
  window.workflow = {
    api,
    state: () => db,
    views,
    selectSession,
    selectProject,
    openPanel,
    refresh: sync,
  };
  checkTeams();
}
initChatControls();
initialize().catch(error);
setInterval(() => {
  if (document.visibilityState === "visible")
    refreshProviderUsageAll().catch(() => {});
}, 120000);

window.addEventListener("keydown", (e) => {
  if (
    e.key === "Escape" &&
    panel === "browser" &&
    browserPage?.picking &&
    !e.target.closest?.(".terminal-host")
  ) {
    e.preventDefault();
    setPicker(true).catch(error);
    return;
  }
  if (e.key === "Escape" && connecting) {
    connecting = null;
    drawConnectionPreview();
  }
});
