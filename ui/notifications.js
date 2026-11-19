"use strict";

// Attention belongs to the live session. Closing a card never answers a prompt.
let notificationState = { enabled: true, entries: [] };
let notificationReady = false;
let notificationCardId = null;
let notificationContextTimer;
let notificationExpiry;
let notificationExpiryId;
let notificationExpiresAt = 0;
let notificationContextBusy = false;
let notificationContextAgain = false;
let lastNotificationContext = "";
let notificationContextToken;
let notificationContextRevision = 0;
const notificationAnnounced = new Set();

function notificationForeground() {
  return notificationState.foreground === true;
}

function notificationLabel(entry) {
  return tr(entry.kind === "approval" ? "Onay bekliyor"
    : entry.kind === "question" ? "Yanıt bekliyor" : "İş tamamlandı");
}

function notificationEntries() {
  return notificationState.enabled ? notificationState.entries.filter(entry => {
    const owner = session(entry.sessionId);
    return owner && !owner.archived && db.projects.some(p => p.id === owner.projectId && !p.removed);
  }) : [];
}

function notificationFollowing() {
  if (!notificationForeground() || $("#dialog").open) return null;
  const owner = session(focusId);
  if (!owner || owner.archived || owner.projectId !== projectId || (owner.folderId || null) !== folderId) return null;
  if (shownChatId === focusId && !$("#chat-view").hidden) return focusId;
  const card = views.get(focusId)?.card;
  if (!card || card.hidden) return null;
  const rect = card.getBoundingClientRect(), viewport = $("#viewport").getBoundingClientRect();
  const width = Math.min(rect.right, viewport.right) - Math.max(rect.left, viewport.left);
  const height = Math.min(rect.bottom, viewport.bottom) - Math.max(rect.top, viewport.top);
  return width > 0 && height > 0 && width * height >= Math.min(rect.width * rect.height, viewport.width * viewport.height) * .5 ? focusId : null;
}

function scheduleNotificationContext() {
  clearTimeout(notificationContextTimer);
  notificationContextTimer = setTimeout(() => updateNotificationContext().catch(() => {}), 60);
}

async function updateNotificationContext(force = false) {
  if (!notificationReady) return;
  if (notificationContextBusy) { notificationContextAgain = true; return; }
  const context = {
    sessionId: notificationFollowing(),
    focused: notificationForeground(),
    language: uiLanguage,
  };
  const key = JSON.stringify(context);
  if (!force && key === lastNotificationContext) return;
  const contextChanged = key !== lastNotificationContext;
  notificationContextBusy = true;
  try {
    const state = await api("notifications", "context", { ...context, contextToken: notificationContextToken,
      contextRevision: ++notificationContextRevision });
    lastNotificationContext = key;
    paintNotifications(state);
    if (contextChanged) { chooseNotificationCard(); paintNotificationCard(); }
    // Native foreground may have changed while this request was in flight.
    if (context.sessionId !== notificationFollowing() || context.focused !== notificationForeground())
      notificationContextAgain = true;
  } finally {
    notificationContextBusy = false;
    if (notificationContextAgain) {
      notificationContextAgain = false;
      scheduleNotificationContext();
    }
  }
}

function notificationProjectBadge(id) {
  const entries = notificationEntries().filter(entry => entry.projectId === id);
  if (!entries.length) return null;
  const waiting = entries.filter(entry => entry.kind !== "completed").length;
  const badge = el("span", "project-attention", waiting ? "!" : "✓");
  badge.dataset.kind = waiting ? "waiting" : "completed";
  badge.setAttribute("aria-hidden", "true");
  return { badge, label: waiting ? tr("{count} oturum yanıt bekliyor", { count: waiting }) : tr("Tamamlanan iş var") };
}

function notificationContextText(entry) {
  const owner = session(entry.sessionId);
  return (db.projects.find(p => p.id === owner?.projectId)?.name || "Workflow")
    + " · " + (owner?.name || tr("Oturum"));
}

async function openNotification(entry) {
  $("#notification-list").hidePopover();
  notificationCardId = null;
  selectSession(entry.sessionId, true);
  if (entry.kind === "completed")
    paintNotifications(await api("notifications", "seen", { id: entry.id }));
  else paintNotificationCard();
  scheduleNotificationContext();
}

async function dismissNotification(entry) {
  notificationCardId = null;
  paintNotificationCard();
  paintNotifications(await api("notifications", "dismiss", { id: entry.id }));
}

function paintNotificationCard() {
  const root = $("#session-notification");
  const entry = notificationEntries().find(item => item.id === notificationCardId && !item.dismissed);
  const followed = entry?.sessionId === notificationFollowing();
  if (!entry || entry.presentation !== "in_app" || !notificationForeground()
      || followed || $("#dialog").open) {
    clearTimeout(notificationExpiry);
    if (followed) notificationCardId = null;
    root.hidden = true;
    return;
  }
  positionNotificationCard();
  if (root.dataset.eventId === entry.id && !root.hidden) {
    paintNotificationMore(root, entry);
    return;
  }
  clearTimeout(notificationExpiry);
  root.dataset.eventId = entry.id;
  root.dataset.kind = entry.kind;
  const heading = el("div", "notification-heading");
  const close = button("×", () => dismissNotification(entry), "icon quiet", tr("Bildirimi kapat"));
  close.setAttribute("aria-label", tr("Bildirimi kapat"));
  heading.append(el("span", "notification-symbol", entry.kind === "completed" ? "✓" : "?"),
    el("strong", "grow", notificationLabel(entry)), close);
  const context = el("p", "notification-context", notificationContextText(entry));
  context.title = context.textContent;
  const footer = el("div", "notification-footer");
  footer.append(el("small", "grow", tr("Şimdi")), button(tr(entry.kind === "completed" ? "Sonucu gör" : "Oturuma git"),
    () => openNotification(entry), "notification-open"));
  root.replaceChildren(heading, context, footer);
  paintNotificationMore(root, entry);
  root.hidden = false;
  api("notifications", "presented", { id: entry.id }).then(state => {
    if (!state.entries.some(item => item.id === entry.id && item.presented)) notificationAnnounced.delete(entry.id);
    paintNotifications(state);
  }).catch(() => notificationAnnounced.delete(entry.id));
  if (entry.kind === "completed") {
    if (notificationExpiryId !== entry.id) {
      notificationExpiryId = entry.id;
      notificationExpiresAt = Date.now() + 9000;
    }
    notificationExpiry = setTimeout(() => dismissNotification(entry).catch(error), Math.max(0, notificationExpiresAt - Date.now()));
  }
}

function paintNotificationMore(root, entry) {
  const count = notificationEntries().filter(item => item.id !== entry.id && item.kind !== "completed").length;
  let more = root.querySelector(".notification-more");
  if (!count) {
    if (more === document.activeElement) root.querySelector(".notification-open")?.focus({ preventScroll: true });
    more?.remove();
    return;
  }
  if (!more) {
    more = button("", () => {
      $("#notification-list").showPopover({ source: $("#notifications-toggle") });
      positionNotificationList();
    }, "quiet notification-more");
    root.append(more);
  }
  more.textContent = tr("Diğer bekleyen oturumlar ({count})", { count });
}

function positionNotificationCard() {
  // Native child WebViews are above DOM layers. Keep the card in the main
  // workspace, clear of both the browser surface and the message composer.
  const root = $("#session-notification");
  const main = $(".main").getBoundingClientRect();
  root.style.right = Math.max(12, innerWidth - main.right + 14) + "px";
  const terminalFooter = !shownChatId && mode === "focus" ? views.get(focusId)?.card.querySelector(".card-footer") : null;
  const bottomEdge = shownChatId ? $("#chat-composer").getBoundingClientRect().top
    : terminalFooter?.getBoundingClientRect().top || $(".main > .footer").getBoundingClientRect().top;
  root.style.bottom = Math.max(16, innerHeight - bottomEdge + 12) + "px";
}

function positionNotificationList() {
  const root = $("#notification-list");
  if (!root.matches(":popover-open")) return;
  const anchor = $("#notifications-toggle").getBoundingClientRect();
  root.style.left = Math.max(8, Math.min(anchor.right - root.offsetWidth, innerWidth - root.offsetWidth - 8)) + "px";
  root.style.top = Math.min(anchor.bottom + 9, innerHeight - root.offsetHeight - 8) + "px";
}

function paintNotifications(state, announce = true) {
  if (!state || !Array.isArray(state.entries)) return;
  if (state.revision < (notificationState.revision || 0)) return;
  const foregroundChanged = state.foreground !== notificationState.foreground;
  notificationState.foreground = state.foreground;
  if (foregroundChanged) paintNotificationCard();
  if (state.revision === notificationState.revision && state.enabled === notificationState.enabled) return;
  const wasEnabled = notificationState.enabled;
  notificationState = state;
  const entries = notificationEntries().sort((a, b) => (a.kind === "completed") - (b.kind === "completed") || b.createdAt - a.createdAt);
  const waiting = entries.filter(entry => entry.kind !== "completed").length;
  const toggle = $("#notifications-toggle"), count = $("#notifications-count");
  count.textContent = entries.length > 99 ? "99+" : String(entries.length);
  count.hidden = !entries.length;
  toggle.dataset.attention = waiting ? "waiting" : entries.length ? "completed" : "none";
  toggle.setAttribute("aria-label", entries.length ? tr("Bildirimler ({count})", { count: entries.length }) : tr("Bildirimler"));
  const list = $("#notification-list");
  const focusedRow = document.activeElement?.closest(".notification-row");
  const focusedId = focusedRow?.dataset.notificationId;
  const focusedClose = document.activeElement?.dataset.notificationClose === "true";
  const header = el("div", "notification-list-heading");
  header.append(el("strong", "grow", tr("Bildirimler")), button("×", () => list.hidePopover(), "icon quiet", tr("Kapat")));
  const body = el("div", "notification-list-body");
  for (const entry of entries) {
    const row = el("div", "notification-row");
    row.dataset.kind = entry.kind;
    row.dataset.notificationId = entry.id;
    const open = button("", () => openNotification(entry), "quiet notification-row-open");
    open.append(el("span", "notification-symbol", entry.kind === "completed" ? "✓" : "?"));
    const text = el("span", "notification-row-text");
    text.append(el("strong", "", notificationLabel(entry)), el("small", "", notificationContextText(entry)));
    open.append(text);
    row.append(open);
    if (entry.kind === "completed") {
      const close = button("×", async () => paintNotifications(await api("notifications", "seen", { id: entry.id })), "icon quiet", tr("Bildirimi kapat"));
      close.setAttribute("aria-label", tr("Bildirimi kapat"));
      close.dataset.notificationClose = "true";
      row.append(close);
    }
    body.append(row);
  }
  if (!entries.length) body.append(el("p", "help", tr(state.enabled ? "Henüz bildirim yok." : "Bildirimler kapalı.")));
  list.replaceChildren(header, body, el("p", "notification-list-help", tr("Bekleyen işaretleri yanıt verdiğinde kalkar.")));
  if (focusedId && list.matches(":popover-open")) {
    const row = [...body.children].find(row => row.dataset.notificationId === focusedId);
    const target = focusedClose ? row?.querySelector("[data-notification-close]") : row?.querySelector("button");
    (target || body.querySelector("button") || $("#notifications-toggle")).focus({ preventScroll: true });
  }
  renderProjectRail();
  if (!state.enabled) notificationCardId = null;
  if (notificationReady && announce && wasEnabled) chooseNotificationCard();
  paintNotificationCard();
  positionNotificationList();
}

function chooseNotificationCard() {
  if (!notificationReady || !notificationForeground() || $("#dialog").open) return;
  const entries = notificationEntries();
  const fresh = entries.filter(entry => entry.presentation === "in_app" && !entry.presented
    && !notificationAnnounced.has(entry.id) && !entry.dismissed && !entry.silent && entry.sessionId !== notificationFollowing())
    .sort((a, b) => (a.kind === "completed") - (b.kind === "completed") || b.createdAt - a.createdAt);
  if (!fresh.length) return;
  const current = entries.find(entry => entry.id === notificationCardId);
  if (!current || current.kind === "completed" || fresh[0].kind !== "completed") {
    for (const entry of fresh) notificationAnnounced.add(entry.id);
    notificationCardId = fresh[0].id;
  }
}

function notificationSettings() {
  const enabled = check(tr("Oturum bildirimleri"), db.preferences.notificationsEnabled !== false);
  enabled.input.id = "notifications-enabled";
  enabled.input.onchange = async () => {
    enabled.input.disabled = true;
    try {
      db = await api("app", "preferences", { notificationsEnabled: enabled.input.checked });
      paintNotifications(await api("notifications", "read"), false);
    } catch (e) {
      enabled.input.checked = db.preferences.notificationsEnabled !== false;
      error(e);
    } finally { enabled.input.disabled = false; }
  };
  const root = el("div", "notification-setting");
  root.append(enabled.label, el("p", "help", tr("Kartlar, proje rozetleri ve görev çubuğu işaretleri. Takip ettiğin oturum için uyarı gösterilmez.")));
  return root;
}

async function initializeNotifications() {
  await listen("notifications-updated", ({ payload }) => paintNotifications(payload));
  const initial = await api("notifications", "attach");
  notificationContextToken = initial.contextToken;
  for (const entry of initial.entries) if (entry.presented) notificationAnnounced.add(entry.id);
  paintNotifications(initial, false);
  notificationReady = true;
  await updateNotificationContext(true);
  $("#notifications-toggle").onclick = () => {
    const list = $("#notification-list");
    list.togglePopover({ source: $("#notifications-toggle") });
    positionNotificationList();
  };
  $("#notification-list").addEventListener("toggle", event =>
    $("#notifications-toggle").setAttribute("aria-expanded", String(event.newState === "open")));
  const changed = () => { chooseNotificationCard(); paintNotificationCard(); scheduleNotificationContext(); };
  window.addEventListener("focus", changed);
  window.addEventListener("blur", () => {
    // Stop using the previous foreground sample immediately. A native child
    // browser can restore the top-level state in the next context response.
    notificationState.foreground = false;
    changed();
  });
  document.addEventListener("visibilitychange", changed);
  window.addEventListener("resize", () => { positionNotificationList(); positionNotificationCard(); scheduleNotificationContext(); });
  new ResizeObserver(positionNotificationCard).observe($("#chat-composer"));
  new ResizeObserver(positionNotificationCard).observe($(".main"));
  $("#viewport").addEventListener("scroll", scheduleNotificationContext);
  new MutationObserver(changed).observe($("#dialog"), { attributes: true, attributeFilter: ["open"] });
  // Refresh focus after native window changes, without querying a model or provider API.
  setInterval(() => updateNotificationContext(true).catch(() => {}), 1500);
}
