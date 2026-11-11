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
  focusId = null,
  panel = "browser",
  providers = [],
  mode = "map",
  camera = { x: 25, y: 40, z: 0.75 },
  gesture = null,
  connecting = null,
  expanded = {},
  browserPage = null,
  browserTabs = [],
  browserJson = null,
  jsonPretty = true,
  jsonVisible = false,
  fileDirectory = "",
  fileScope = "",
  fileSelection = null,
  fileReadRevision = 0,
  changeFile = null,
  changeDiff = null,
  gitState = null,
  historyCommits = [],
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
        closeModal();
      } catch (e) {
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
  return db.sessions.filter((s) => s.projectId === projectId && !s.archived);
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
  projectId = id;
  const sessions = visibleSessions();
  if (!sessions.some((s) => s.id === focusId))
    focusId = sessions[0]?.id || null;
  fileDirectory = "";
  browserPage = null;
  render();
  refreshPanel();
  remember();
}
function selectSession(id, center = false, tab = null) {
  const s = session(id);
  if (!s) return;
  projectId = s.projectId;
  focusId = id;
  render();
  if (tab) openPanel(tab).catch(error);
  else refreshPanel();
  remember();
  if (center) focusCard();
}
function render() {
  db.projects ??= [];
  db.sessions ??= [];
  db.links ??= [];
  db.jobs ??= [];
  db.active ??= [];
  if (!project() || project()?.removed)
    projectId = db.projects.find((p) => !p.removed)?.id || null;
  if (
    !session(focusId) ||
    session(focusId)?.projectId !== projectId ||
    session(focusId)?.archived
  )
    focusId = visibleSessions()[0]?.id || null;
  renderProjects();
  renderCards();
  renderProviderCards();
  $("#project-title").textContent = project()?.name || tr("Çalışma alanı");
  $("#project-title").title = $("#project-title").textContent;
  const count = visibleSessions().length;
  $("#session-count").textContent = countLabel(count, "{count} oturum");
  $("#panel-context").textContent =
    (project()?.name || "") +
    (session(focusId) ? " · " + session(focusId).name : "");
  $("#running-status span").textContent = db.active.length
    ? countLabel(db.active.length, "{count} oturum çalışıyor")
    : tr("Çalışan oturum yok");
  $("#empty").hidden = visibleSessions().length > 0;
  $("#empty").style.marginTop = "100px";
  const options = visibleSessions().map((s) => ({
    id: s.id,
    label: names[s.provider] + " · " + s.name,
  }));
  const bs = $("#browser-session");
  bs.replaceChildren(...options.map((o) => new Option(o.label, o.id)));
  bs.value = focusId || "";
  paintBrowserPermission();
  if (panel === "links") renderLinks();
}
function renderProjects() {
  const root = $("#projects");
  root.replaceChildren();
  for (const p of db.projects.filter((p) => !p.removed)) {
    const item = el("section", "project-item"),
      row = el("div", "project-row" + (p.id === projectId ? " active" : ""));
    const sessions = db.sessions.filter(
      (s) => s.projectId === p.id && !s.archived,
    );
    const open = expanded[p.id] !== false;
    row.append(
      button(
        open ? "⌄" : "›",
        () => {
          expanded[p.id] = !open;
          renderProjects();
          remember();
        },
        "icon quiet",
      ),
      el("span", "muted", "▱"),
      el("span", "project-name", p.name),
      el("small", "", sessions.length),
      button(
        "×",
        () => {
          modal(
            tr("Projeyi listeden kaldır"),
            el(
              "p",
              "help",
              p.name +
                tr(" listeden kaldırılacak. Proje dosyaları korunur. Açık oturumlarını önce durdur."),
            ),
            async () => {
              await api("app", "remove_project", { id: p.id });
              if (projectId === p.id) projectId = null;
              await sync();
            },
            tr("Kaldır"),
          );
        },
        "icon quiet",
        tr("Projeyi kaldır"),
      ),
    );
    row.onclick = () => selectProject(p.id);
    item.append(row);
    if (open) {
      const children = el("div", "project-children");
      if (!sessions.length)
        children.append(el("p", "help", tr("Henüz oturum yok.")));
      for (const s of sessions) {
        const b = button(
          "",
          () => selectSession(s.id),
          "session-entry" + (s.id === focusId ? " active" : ""),
        );
        b.append(
          el("span", "provider-icon " + s.provider, icons[s.provider]),
          el("span", "name", s.name),
          stateBadge(s.id),
        );
        b.ondblclick = () => rename(s.id);
        children.append(b);
      }
      item.append(children);
    }
    root.append(item);
  }
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
      !$("#provider-cards").hidden
    )
      refreshProviderUsage(id, accountId);
  }
}
async function refreshProviderUsageAll() {
  if (!$("#provider-cards").hidden)
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
    v.path.textContent =
      (s.workspaceKind === "direct"
        ? tr("Mevcut klasör")
        : s.workspaceKind === "worktree"
          ? "Git worktree"
          : tr("Ayrı klasör")) +
      " · " +
      s.directory;
    const account = providerAccount(s.provider, s.accountId || defaultAccount);
    v.account.hidden =
      (!s.accountId || s.accountId === defaultAccount) &&
      (providerInfo(s.provider)?.accounts.length || 0) < 2;
    v.account.textContent = account ? accountLabel(account) : tr("Hesap");
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
    button("⋯", () => sessionMenu(s.id), "icon", tr("Oturum seçenekleri")),
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
    account = el("span", "card-account"),
    summary = button(
      tr("Özet"),
      () => selectSession(s.id, false, "summary"),
      "card-summary",
    );
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
  term.onData((data) => {
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
    api("terminal", "write", { id: s.id, runId: v.runId, data }).catch(error);
  });
  term.attachCustomKeyEventHandler((e) => {
    if (
      e.type === "keydown" &&
      e.shiftKey &&
      ["PageUp", "PageDown"].includes(e.key)
    ) {
      term.scrollPages(e.key === "PageUp" ? -1 : 1);
      return false;
    }
    return true;
  });
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
      renderProjects();
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
    renderProjects();
    const view = views.get(id);
    if (view) view.state.replaceWith((view.state = stateBadge(id)));
    if (["permission", "completed"].includes(s.key) && old)
      notifySession(id, s.label);
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
function notifySession(id, label) {
  if (document.hasFocus() && focusId === id) return;
  api("app", "notify", { id, label }).catch(() => {});
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
async function archiveSession(id) {
  if (active(id)) {
    modal(
      tr("Oturumu arşivle"),
      el(
        "p",
        "help",
        tr("Önce bu oturumun terminalini durdur. Konuşma ve çalışma alanı arşivde korunur."),
      ),
      async () => {
        await api("terminal", "stop", { id, runId: views.get(id)?.runId });
        await new Promise((r) => setTimeout(r, 800));
        await api("app", "session_patch", { id, archived: true });
        await sync();
      },
      tr("Durdur ve arşivle"),
    );
    return;
  }
  await api("app", "session_patch", { id, archived: true });
  await sync();
}
async function sessionMenu(id) {
  const s = session(id),
    box = el("div"),
    bind = button(tr("Konuşma seç / doğrula"), () => bindHistory(id));
  if (active(id) && s.conversationId) {
    bind.disabled = true;
    bind.title = tr("Konuşmayı değiştirmek için önce oturumu durdur");
  }
  box.append(
    button(tr("Konuşma geçmişini oku"), () => showTranscript(id)),
    bind,
    ...(s.workspaceKind === "direct"
      ? []
      : [button(tr("Eklentiler"), () => sessionAddons(id))]),
    ...(s.login ? [] : [button(tr("Önizleme ayarları"), () => previewSettings(s))]),
    button(tr("Çalışma klasörünü aç"), () =>
      api("app", "reveal", { sessionId: id }),
    ),
  );
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
  box.append(details);
  modal(s.name, box);
}
async function showTranscript(id) {
  const result = await api("history", "messages", { id });
  const box = el("div");
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
  if (view.inputDraft.trim())
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
  $("#world").style.transform =
    `translate(${camera.x}px,${camera.y}px) scale(${camera.z})`;
  $("#zoom").textContent = Math.round(camera.z * 100) + "%";
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
  remember();
}
function zoom(amount, x, y) {
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
function drawEdges() {
  const group = $("#edge-lines");
  group.replaceChildren();
  for (const { card } of views.values()) card.classList.remove("linked-focus");
  if (mode !== "map") return;
  const highlight = panel === "summary" && focusId;
  for (const link of db.links) {
    const a = views.get(link.source)?.card,
      b = views.get(link.target)?.card;
    if (!a || !b || a.hidden || b.hidden) continue;
    const ax = parseFloat(a.style.left),
      ay = parseFloat(a.style.top),
      bx = parseFloat(b.style.left),
      by = parseFloat(b.style.top);
    const x1 = ax + a.offsetWidth,
      y1 = ay + 35,
      x2 = bx,
      y2 = by + 35;
    const bend = Math.max(70, Math.abs(x2 - x1) / 2);
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute(
      "d",
      `M${x1} ${y1}C${x1 + bend} ${y1},${x2 - bend} ${y2},${x2} ${y2}`,
    );
    const focused =
      highlight && (link.source === focusId || link.target === focusId);
    const emphasis = focused ? " edge-focused" : highlight ? " edge-muted" : "";
    if (focused) (link.source === focusId ? b : a).classList.add("linked-focus");
    path.setAttribute("class", "edge" + emphasis);
    path.setAttribute("marker-end", "url(#arrow)");
    path.onclick = () => openLink(link.source, link.target);
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "g");
    icon.setAttribute("class", "link-icon" + emphasis);
    icon.setAttribute(
      "transform",
      `translate(${(x1 + x2) / 2},${(y1 + y2) / 2})`,
    );
    const circle = document.createElementNS(icon.namespaceURI, "circle");
    circle.setAttribute("r", "16");
    const t = document.createElementNS(icon.namespaceURI, "text");
    t.textContent = "⛓";
    icon.append(circle, t);
    icon.onclick = () => openLink(link.source, link.target);
    group.append(path, icon);
  }
}
$("#viewport").addEventListener("pointerdown", (e) => {
  if (
    mode !== "map" ||
    e.button !== 0 ||
    e.target.closest(".session-card") ||
    e.target.closest(".link-icon")
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
    const target = document
      .elementFromPoint(e.clientX, e.clientY)
      ?.closest(".session-card")?.dataset.id;
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
    if (mode !== "map" || !e.ctrlKey) return;
    e.preventDefault();
    const r = $("#viewport").getBoundingClientRect();
    zoom(Math.exp(-e.deltaY * 0.002), e.clientX - r.left, e.clientY - r.top);
  },
  { passive: false },
);
$("#zoom-out").onclick = () => zoom(1 / 1.15);
$("#zoom-in").onclick = () => zoom(1.15);
$("#fit").onclick = fitMap;
$("#focus-card").onclick = focusCard;
for (const m of ["map", "grid", "focus"])
  $("#view-" + m).onclick = () => setMode(m);
function remember() {
  localStorage.setItem(
    "agentloom-layout",
    JSON.stringify({
      projectId,
      focusId,
      mode,
      camera,
      expanded,
      panel,
      noLeft: document.body.classList.contains("no-left"),
      noRight: document.body.classList.contains("no-right"),
      showProviders: !$("#provider-cards").hidden,
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
  modal(tr("Ayarlar"), field(tr("Arayüz dili"), language));
  $("#dialog-footer button").textContent = tr("Kapat");
};
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
    dividers = $("#left-resizer").offsetWidth + $("#right-resizer").offsetWidth,
    minimum = parseFloat(style.getPropertyValue(panel.variable + "-min")),
    center = parseFloat(style.getPropertyValue("--main-min"));
  const root = document.documentElement.style;
  if (otherWidth) root.setProperty(other.variable, otherWidth + "px");
  root.setProperty(
    panel.variable,
    Math.max(
      minimum,
      Math.min(width, layout.clientWidth - center - otherWidth - dividers),
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
  for (const account of providerInfo(provider).accounts) {
    const row = el("div", "row account-manager-row");
    const label = el("div", "grow");
    label.append(
      el("strong", "", accountLabel(account)),
      el("small", "", account.id === defaultAccount
        ? tr("Bilgisayardaki mevcut sağlayıcı girişi")
        : account.verified ? tr("Hesap bağlandı") : tr("Giriş bekleniyor")),
    );
    row.append(label, button(
      account.verified ? tr("Girişi aç") : tr("Hesabı bağla"),
      () => connect(account.id),
    ));
    list.append(row);
  }
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
  list.append(field(tr("Yeni hesap adı"), name), add);
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
  let inputQueue = Promise.resolve();
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
    term.onData((data) => {
      if (ended || /^\x1b\[[?0-9;]*[Rc]$/.test(data)) return;
      inputQueue = inputQueue
        .then(() => {
          if (!closed && !ended) return call("write", { data });
        })
        .catch((e) => {
          if (!closed && !ended) error(e);
        });
    });
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
  projects.onchange = workspaceMode.onchange = paintWorkspace;
  paintWorkspace();
  modal(
    tr("Yeni oturum"),
    [
      field(tr("Proje"), projects),
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
    row.append(
      button(
        a.installed ? tr("Kurulu · Yeniden kur") : tr("Kur"),
        async (e) => {
          const b = e.currentTarget;
          b.disabled = true;
          b.textContent = tr("Kuruluyor…");
          try {
            await api("addons", "install", { id: a.id });
            b.textContent = tr("Kuruldu");
            toast(a.name + tr(" kütüphaneye kuruldu"));
          } finally {
            b.disabled = false;
          }
        },
        "primary",
      ),
    );
    box.append(row);
  }
  box.append(
    el(
      "p",
      "help",
      tr("Kurmak tüm oturumlarda etkinleştirmez. Oturum oluştururken veya oturumun ⋯ menüsündeki Eklentiler bölümünden seç. OpenSpec’in kendi CLI’sı için bilgisayardaki Node.js/npm kullanılır."),
    ),
  );
  modal(tr("Eklenti kütüphanesi"), box);
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
async function archiveList() {
  const box = el("div");
  for (const s of db.sessions.filter((s) => s.archived)) {
    const row = el("div", "archive-row");
    row.append(
      el("span", "", s.name + " · " + names[s.provider]),
      button(tr("Geçmiş"), () => showTranscript(s.id)),
      button(tr("Geri getir"), async () => {
        await api("app", "session_patch", { id: s.id, archived: false });
        closeModal();
        await sync();
        selectSession(s.id);
      }),
    );
    box.append(row);
  }
  if (!box.childNodes.length)
    box.append(el("p", "help", tr("Arşivlenmiş oturum yok.")));
  modal(tr("Geçmiş oturumlar"), box);
}
$("#new-session").onclick = () => newSession().catch(error);
$("#add-project").onclick = () => newProject().catch(error);
$("#addons").onclick = () => addonLibrary().catch(error);
$("#archive").onclick = () => archiveList().catch(error);
$("#refresh").onclick = busy($("#refresh"), async () => {
  await scanProviders();
  await sync();
  await refreshPanel();
});
$("#show-providers").onclick = () => {
  $("#provider-cards").hidden = !$("#provider-cards").hidden;
  remember();
  refreshProviderUsageAll().catch(() => {});
  fitTerminals();
};

async function openPanel(tab, reveal = true) {
  panel = tab;
  if (reveal) document.body.classList.remove("no-right");
  for (const n of ["browser", "files", "changes", "git", "links", "summary"])
    $("#panel-" + n).hidden = n !== tab;
  $$("#panel-tabs button").forEach((b) =>
    b.classList.toggle("selected", b.dataset.tab === tab),
  );
  $("#panel-title").textContent = {
    browser: tr("Tarayıcı"),
    files: tr("Dosyalar"),
    changes: tr("Değişiklikler"),
    git: "Git",
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
  if (!projectId && panel !== "files") return;
  try {
    if (panel === "files") await loadFiles(token);
    if (panel === "changes") await loadChanges(token);
    if (panel === "git") await loadGit(token);
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
    body.append(...diffRows(result));
    diff.replaceChildren(el("h4", "", path), body);
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
async function loadChanges(token = panelGeneration) {
  const context = ctx();
  const direct = session(focusId)?.workspaceKind === "direct";
  $("#transfer-open").hidden = $("#transfer-undo").hidden = direct;
  $("#restore-undo").disabled = !lastRestore || lastRestore.sessionId !== focusId;
  if (!focusId) {
    $("#change-list").replaceChildren(el("p", "empty", tr("Bir oturum seç.")));
    return;
  }
  const [rows, transfers] = await Promise.all([
    api("files", "changes", context),
    direct ? [] : api("files", "transfer_history", context),
  ]);
  if (token !== panelGeneration) return;
  lastTransfer =
    transfers.find((t) => ["applied", "prepared", "reverting"].includes(t.state)) ||
    null;
  $("#transfer-undo").disabled = !lastTransfer;
  $("#change-list").replaceChildren();
  for (const item of rows) {
    const row = el("div", "file-row");
    row.append(el("span", "status", item.status), el("code", "", item.path));
    row.onclick = () => showChange(item.path).catch(error);
    $("#change-list").append(row);
  }
  if (!rows.length)
    $("#change-list").append(
      el("p", "empty", tr("Başlangıç kaydına göre değişiklik yok.")),
    );
  if (changeFile && rows.some((r) => r.path === changeFile))
    await showChange(changeFile);
  else {
    $("#change-diff").replaceChildren();
    $("#restore-file").disabled = true;
    $("#restore-lines").disabled = true;
  }
}
async function showChange(path) {
  const context = ctx(),
    token = panelGeneration,
    result = await api("files", "diff", { ...context, path });
  if (token !== panelGeneration) return;
  changeFile = path;
  changeDiff = result;
  $("#change-file").textContent = path;
  $("#restore-file").disabled = false;
  $("#restore-lines").disabled = true;
  $("#change-diff").replaceChildren(
    ...diffRows(result, () => {
      $("#restore-lines").disabled = !$$("#change-diff input:checked").length;
    }),
  );
}
function diffRows(result, onSelect) {
  if (result.binary) return [el("p", "empty", tr("İkili dosya değişikliği"))];
  return result.lines.map((line) => {
    const row = el("div", "diff-line " + line.tag);
    if (onSelect && line.tag !== "equal") {
      const c = el("input");
      c.type = "checkbox";
      c.dataset.index = line.index;
      c.onchange = onSelect;
      row.append(c);
    } else row.append(el("span"));
    row.append(
      el("span", "number", line.old ?? ""),
      el("span", "number", line.new ?? ""),
      el("span", "", line.text.replace(/\n$/, "")),
    );
    return row;
  });
}
async function restore(selected) {
  if (!changeFile) return;
  const indices = selected
    ? $$("#change-diff input:checked").map((c) => Number(c.dataset.index))
    : undefined;
  const context = ctx(),
    path = changeFile,
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
      await loadChanges();
    },
    tr("Geri al"),
  );
}
$("#changes-refresh").onclick = () => loadChanges().catch(error);
$("#restore-file").onclick = () => restore(false);
$("#restore-lines").onclick = () => restore(true);
$("#restore-undo").onclick = async () => {
  try {
    await api("files", "undo_restore", { ...ctx(), token: lastRestore.id });
    $("#restore-undo").disabled = true;
    await loadChanges();
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

function patchView(target, patch) {
  target.replaceChildren();
  for (const line of String(patch || tr("Değişiklik yok.")).split("\n")) {
    target.append(
      el(
        "div",
        "diff-plain " +
          (line.startsWith("@@")
            ? "hunk"
            : line.startsWith("+") && !line.startsWith("+++")
              ? "insert"
              : line.startsWith("-") && !line.startsWith("---")
                ? "delete"
                : ""),
        line || " ",
      ),
    );
  }
}
async function loadGit(token = panelGeneration) {
  const context = ctx(),
    state = await api("git", "state", context);
  if (token !== panelGeneration) return;
  gitState = state;
  $("#git-empty").hidden = state.available;
  $("#git-sections").hidden = !state.available;
  $("#git-branch").textContent = state.available
    ? "⑂ " + state.branch
    : tr("Git kullanılmıyor");
  $("#git-target").textContent =
    state.upstream || tr("Push hedefi: origin / ") + (state.branch || "");
  if (!state.available) return;
  $("#git-staged").replaceChildren();
  $("#git-working").replaceChildren();
  let staged = 0,
    working = 0;
  for (const file of state.files) {
    for (const isStaged of [true, false]) {
      const include = isStaged
        ? file.index !== " " && file.index !== "?"
        : file.work !== " " || file.untracked;
      if (!include) continue;
      if (isStaged) staged++;
      else working++;
      const row = el("div", "file-row"),
        box = el("input");
      box.type = "checkbox";
      box.checked = gitSelected.has(file.path);
      box.onchange = () =>
        box.checked
          ? gitSelected.add(file.path)
          : gitSelected.delete(file.path);
      const mark = el(
        "span",
        "status",
        file.conflict
          ? "!"
          : file.untracked
            ? "U"
            : isStaged
              ? file.index
              : file.work,
      );
      mark.title = file.untracked
        ? tr("Untracked · Git tarafından henüz izlenmiyor")
        : file.conflict
          ? tr("Çakışma")
          : tr("Dosya değişti");
      row.append(box, mark, el("code", "", file.path));
      row.append(
        button(
          isStaged ? "−" : "+",
          () => stage([file.path], isStaged),
          "icon",
          isStaged ? tr("Hazırlananlardan çıkar") : tr("Commit için hazırla"),
        ),
      );
      row.onclick = async () => {
        try {
          if (file.conflict) {
            await resolveConflict(file);
            return;
          }
          const d = await api("git", "diff", {
            ...context,
            path: file.path,
            staged: isStaged,
          });
          $("#git-diff-title").textContent =
            file.path + (isStaged ? tr(" · Hazırlandı") : tr(" · Değişiklik"));
          patchView($("#git-diff"), d.patch);
        } catch (e) {
          error(e);
        }
      };
      (isStaged ? $("#git-staged") : $("#git-working")).append(row);
    }
  }
  $("#staged-count").textContent = staged;
  $("#working-count").textContent = working;
  $("#git-commit").disabled = !staged;
  if (!staged)
    $("#git-staged").append(el("p", "help", tr("Henüz hazırlanmış dosya yok.")));
  if (!working) $("#git-working").append(el("p", "help", tr("Değişiklik yok.")));
  const choice = $("#history-branch"),
    value = choice.value;
  choice.replaceChildren(
    new Option(tr("Tüm dallar"), "--all"),
    ...state.branches.map((b) => new Option(b.name, b.name)),
  );
  choice.value = [...choice.options].some((o) => o.value === value)
    ? value
    : "--all";
  await loadHistory(context, token);
}
async function stage(files, unstage = false) {
  if (!gitState) return;
  const context = ctx();
  await api("git", "stage", {
    ...context,
    stamp: gitState.stamp,
    files,
    unstage,
  });
  gitSelected.clear();
  await loadGit();
  if (!unstage && files.length > 1 && $("#suggest-after-stage").checked)
    await suggestCommit();
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
  const result = await api("git", "commit", {
    ...ctx(),
    stamp: gitState.stamp,
    message: $("#commit-message").value,
  });
  $("#commit-message").value = "";
  toast(result.output || tr("Commit oluşturuldu"));
  await loadGit();
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
  await loadGit();
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
  await loadGit();
});
async function suggestCommit() {
  if (!focusId || !active(focusId))
    throw Error(tr("Mesaj önerisi için seçili oturumu başlat"));
  const v = views.get(focusId);
  if (v.inputDraft || ["working", "permission"].includes(status(focusId).key))
    throw Error(tr("Ajan hazır olmalı ve giriş taslağı boş olmalı"));
  const message =
    "Git indexine hazırlanmış değişiklikleri incele. Yalnızca uygun bir commit mesajı öner. Commit veya push yapma; dosyaları değiştirme.";
  await pasteDraft(focusId, message);
  toast(
    tr("İstek terminale taslak olarak eklendi. Gönderdikten sonra önerilen mesajı commit alanına yazabilirsin."),
  );
}
$("#git-suggest").onclick = () => suggestCommit().catch(error);
$("#suggest-after-stage").checked =
  localStorage.getItem("agentloom-suggest") === "true";
$("#suggest-after-stage").onchange = (e) =>
  localStorage.setItem("agentloom-suggest", String(e.target.checked));
$("#git-more").onclick = () => {
  if (!gitState?.available) return;
  const box = el("div");
  for (const [id, label] of [
    ["branch", tr("Yeni dal")],
    ["switch", tr("Dal değiştir")],
    ["merge", tr("Dal birleştir")],
    ["pr", tr("Taslak PR oluştur")],
    ["merge_continue", tr("Birleştirmeyi tamamla")],
    ["merge_abort", tr("Birleştirmeyi iptal et")],
  ])
    box.append(button(label, () => gitForm(id)));
  modal(tr("Git işlemleri"), box);
};
function gitForm(action) {
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
        ...ctx(),
        stamp: gitState.stamp,
        ...request,
      });
      toast(r.output || tr("İşlem tamamlandı"));
      await loadGit();
    },
    tr("Uygula"),
  );
}
async function resolveConflict(file) {
  const context = ctx(),
    data = await api("git", "conflict_details", { ...context, path: file.path }),
    choice = select([
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
        stamp: gitState.stamp,
        path: file.path,
        choice: choice.value,
        text: editor.value,
        fileStamp: data.hash,
      });
      await loadGit();
    },
    tr("Çöz ve hazırla"),
  );
}
async function loadHistory(context = ctx(), token = panelGeneration) {
  historyCommits = await api("git", "history", {
    ...context,
    branch: $("#history-branch").value,
  });
  if (token !== panelGeneration) return;
  const root = $("#git-history");
  root.replaceChildren();
  let lanes = [];
  const colors = ["#a6d47e", "#a4bbee", "#d2a0de", "#e4bd78", "#77c6bc"];
  for (const commit of historyCommits) {
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
    const row = el(
      "div",
      "history-row" +
        (selectedCommit === commit.hash ? " active" : "") +
        (baseCommit === commit.hash ? " base" : ""),
    );
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
    row.onclick = (e) => {
      if (e.ctrlKey && selectedCommit && selectedCommit !== commit.hash)
        baseCommit = commit.hash;
      else {
        selectedCommit = commit.hash;
        baseCommit = null;
      }
      showCommit().catch(error);
      loadHistory().catch(error);
    };
    root.append(row);
  }
}
$("#history-refresh").onclick = () => loadHistory().catch(error);
$("#history-branch").onchange = () => loadHistory().catch(error);
async function showCommit() {
  const context = ctx(),
    selected = selectedCommit,
    base = baseCommit,
    rows = await api("git", "commit_files", {
      ...context,
      commit: selected,
      from: base,
    });
  if (selected !== selectedCommit || base !== baseCommit) return;
  const box = el("div"),
    filesBox = el("div"),
    diffBox = el("div");
  filesBox.style.cssText =
    "max-height:135px;overflow:auto;border-bottom:1px solid var(--line)";
  box.append(
    el(
      "p",
      "help",
      base ? base.slice(0, 8) + " → " + selected.slice(0, 8) : selected,
    ),
    filesBox,
    diffBox,
  );
  for (const f of rows) {
    const row = el("div", "file-row");
    row.append(el("span", "status", f.status), el("code", "", f.path));
    row.onclick = async () => {
      try {
        const d = await api("git", "commit_diff", {
          ...context,
          commit: selected,
          from: base,
          path: f.path,
        });
        patchView(diffBox, d.patch);
      } catch (e) {
        error(e);
      }
    };
    filesBox.append(row);
  }
  if (!rows.length) filesBox.append(el("p", "empty", tr("Dosya farkı yok.")));
  $("#git-diff-title").textContent = base
    ? tr("İki commit arasındaki fark")
    : tr("Commit değişiklikleri");
  $("#git-diff").replaceChildren(box);
}

async function openLink(source, target) {
  if (!source) {
    source = focusId;
  }
  const candidates = visibleSessions().filter((s) => s.id !== source),
    choice = select(
      candidates.map((s) => ({
        id: s.id,
        label: s.name + " · " + names[s.provider],
      })),
      target,
    ),
    existing = db.links.find(
      (l) => l.source === source && l.target === (target || choice.value),
    ),
    read = check(tr("Konuşma geçmişini okuyabilsin"), existing?.read ?? true),
    files = check(tr("Dosyalarını okuyabilsin"), existing?.files ?? false),
    work = check(tr("Bu oturumdan iş isteyebilsin"), existing?.work ?? false),
    box = el("div");
  box.append(
    el(
      "p",
      "help",
      (session(source)?.name || tr("Oturum")) +
        tr(" → seçilen oturum. İzin tek yönlüdür; dosyalar ortak hale gelmez."),
    ),
    field(tr("Hedef oturum"), choice),
    read.label,
    files.label,
    work.label,
  );
  if (existing)
    box.append(
      button(
        tr("Bağlantıyı kes"),
        async () => {
          await api("app", "unlink", { id: existing.id });
          closeModal();
          await sync();
        },
        "danger",
      ),
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
    v.inputDraft.trim() ||
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
    $("#dialog").open ||
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
          $("#dialog").open ||
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
      ({ projectId, focusId, mode, camera, expanded } = Object.assign(
        { expanded: {}, camera },
        saved,
      ));
      panel = saved.panel || "browser";
      document.body.classList.toggle("no-left", !!saved.noLeft);
      document.body.classList.toggle("no-right", !!saved.noRight);
      $("#provider-cards").hidden = !saved.showProviders;
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
  for (const [id, view] of views) paintPreview(id, view);
  await scanProviders();
  setMode(mode);
  await openPanel(panel, false);
  window.addEventListener("resize", () => {
    fitTerminals();
    scheduleViewport();
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
}
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
