"use strict";

const chatViews = new Map();
let shownChatId = null;
let chatPollTimer;
let chatInteractionTimer;

function scheduleChatInteraction(delay = 0) {
  if (chatInteractionTimer !== undefined || !shownChatId) return;
  chatInteractionTimer = setTimeout(() => {
    chatInteractionTimer = undefined;
    readChatInteraction().catch(error);
  }, delay);
}

async function readChatInteraction() {
  const id = shownChatId;
  if (!id) return;
  const view = chatView(id);
  if (view.readingInteraction) return;
  const bindingRevision = view.bindingRevision;
  const readRevision = view.readRevision || 0;
  const promptRevision = view.promptRevision || 0;
  view.readingInteraction = true;
  try {
    // Native questions must never wait for transcript discovery or filesystem IO.
    const update = await api("chat", "interaction", { id, terminalOutput: view.terminalPreviewOpen === true });
    if (!session(id) || shownChatId !== id || (view.readRevision || 0) !== readRevision
      || view.bindingRevision !== bindingRevision || (view.promptRevision || 0) !== promptRevision
      || view.conversation !== update.conversationId
      || (session(id).conversationId || null) !== update.conversationId) return;
    view.mismatch = update.mismatch;
    view.ticket = update.ticket;
    view.liveApproval = update.liveApprovalMode;
    if (!view.answeringPrompt) view.terminalPrompt = update.terminalPrompt;
    view.terminalOutput = update.terminalOutput;
    view.terminalRunId = update.terminalRunId;
  } catch {
    if (shownChatId === id && (view.readRevision || 0) === readRevision
      && view.bindingRevision === bindingRevision && (view.promptRevision || 0) === promptRevision) {
      view.ticket = null;
      view.terminalPrompt = null;
      view.terminalOutput = null;
      view.terminalRunId = null;
      view.liveApproval = null;
    }
  } finally {
    view.readingInteraction = false;
    if (shownChatId === id) {
      paintChatComposer(session(id), view);
      scheduleChatInteraction(active(id) ? 300 : 850);
    } else if (shownChatId) scheduleChatInteraction();
  }
}

function selectedChatSession() {
  const selected = session(focusId);
  return selected && ["codex", "claude", "copilot"].includes(selected.provider) && !selected.login && !selected.archived ? selected : null;
}

function chatView(id) {
  if (!chatViews.has(id))
    chatViews.set(id, { draft: "", images: [], open: false, turns: [], ticket: null, revision: null, bindingRevision: 0, terminalPreviewOpen: false });
  return chatViews.get(id);
}

function openChatChanges() {
  changesPreference = "session";
  changesHistory = false;
  return openPanel("changes");
}

function setChatView(open) {
  const selected = selectedChatSession();
  if (!selected) return;
  chatView(selected.id).open = open;
  renderChatView();
  scheduleTerminalRendering();
  if (open) openChatChanges().catch(error);
  else requestAnimationFrame(fitTerminals);
}

function scheduleChatRead(delay = 0) {
  if (chatPollTimer !== undefined || !shownChatId) return;
  chatPollTimer = setTimeout(() => {
    chatPollTimer = undefined;
    readChat().catch(error);
  }, delay);
}

function renderChatView() {
  const selected = selectedChatSession();
  const next = selected && chatViews.get(selected.id)?.open ? selected.id : null;
  const switched = shownChatId !== next;
  const area = $("#chat-view");
  const world = $("#world");
  if (switched) $("#chat-model-menu").hidePopover();
  $("#session-view-switch").hidden = !selected;
  $("#session-terminal-view").setAttribute("aria-pressed", String(!next));
  $("#session-chat-view").setAttribute("aria-pressed", String(!!next));
  area.hidden = !next;
  world.inert = !!next;
  $("#viewport").classList.toggle("show-chat", !!next);
  if (next) $("#empty").hidden = true;
  shownChatId = next;
  renderVerificationView();
  scheduleNotificationContext();
  if (switched) {
    clearTimeout(chatPollTimer);
    chatPollTimer = undefined;
    clearTimeout(chatInteractionTimer);
    chatInteractionTimer = undefined;
  }
  for (const [id, view] of chatViews) {
    const owner = session(id);
    if (!owner || owner.archived) chatViews.delete(id);
    else if (id !== next) {
      syncChatBinding(owner, view);
      view.readRevision = (view.readRevision || 0) + 1;
      // Preserve drafts, attachments and answers; release inactive transcripts.
      if (view.turns.length || view.revision || view.terminalOutput) {
        view.turns = [];
        view.revision = null;
        view.terminalOutput = null;
      }
    }
  }
  if (!next) return;
  const view = chatView(next);
  const conversationChanged = syncChatBinding(selected, view);
  if (switched || conversationChanged) {
    $("#chat-input").value = view.draft;
    paintChatImages(view);
    paintChatTurns(view, true);
    scheduleChatRead();
  }
  if (active(next)) scheduleChatRead();
  scheduleChatInteraction();
  paintChatComposer(selected, view);
}

function syncChatBinding(selected, view) {
  const conversation = selected.conversationId || null;
  if (view.conversation === conversation) return false;
  view.bindingRevision++;
  if (view.conversation) {
    const images = view.images;
    view.draft = "";
    view.images = [];
    view.pending = null;
    if (!view.sending) Promise.all(images.map(image => discardScreenshot(selected.id, image))).catch(error);
  }
  view.conversation = conversation;
  view.revision = null;
  view.turns = [];
  view.ticket = null;
  view.liveApproval = null;
  view.terminalPrompt = null;
  view.terminalOutput = null;
  view.terminalRunId = null;
  view.terminalPreviewOpen = false;
  view.promptRevision = (view.promptRevision || 0) + 1;
  view.questionReply = null;
  view.answeredQuestionKey = null;
  return true;
}

async function readChat() {
  const id = shownChatId;
  if (!id) return;
  const view = chatView(id);
  if (view.reading) return;
  const bindingRevision = view.bindingRevision;
  const readRevision = view.readRevision || 0;
  view.reading = true;
  try {
    const update = await api("chat", "read", { id, revision: view.revision, terminalOutput: !!view.terminalPreviewOpen });
    if (!session(id) || shownChatId !== id || (view.readRevision || 0) !== readRevision
      || view.bindingRevision !== bindingRevision || view.conversation !== update.conversationId
      || (session(id).conversationId || null) !== update.conversationId) return;
    const last = view.turns.at(-1);
    const wasWorking = last?.state === "working" || !!view.pending;
    view.found = update.found;
    view.loadError = !!update.logError;
    if (Array.isArray(update.turns)) {
      const next = update.turns.at(-1);
      const changed = view.revision !== update.revision || view.turns.length !== update.turns.length;
      view.turns = update.turns;
      view.revision = update.revision;
      const acknowledged = reconcileChatPrompt(view);
      if (shownChatId === id && (changed || acknowledged)) {
        paintChatTurns(view);
        if (wasWorking && next?.state === "completed" && panel === "changes" && changesPreference === "session")
          refreshPanel().catch(error);
      }
    }
    if (view.pending && !active(id)) {
      view.pending = null;
      if (shownChatId === id) paintChatTurns(view);
    }
  } catch {
    if (shownChatId === id && (view.readRevision || 0) === readRevision && view.bindingRevision === bindingRevision) view.loadError = true;
  } finally {
    view.reading = false;
    if (shownChatId === id) {
      paintChatEmpty(view);
      paintChatComposer(session(id), view);
      if (active(id) || view.loadError) scheduleChatRead(850);
    } else if (shownChatId) scheduleChatRead();
  }
}

function paintChatEmpty(view) {
  const empty = $("#chat-empty");
  empty.hidden = view.turns.length > 0 || !!view.pending;
  if (!empty.hidden) {
    empty.textContent = tr(view.mismatch
      ? "Bu konuşmayı Terminal görünümünde bağlayıp yeniden başlat."
      : view.loadError
        ? "Sohbet kaydı yüklenemedi."
        : "Mesajlar konuşma kaydına yazıldığında burada görünür.");
  }
}

function paintChatComposer(selected, chat) {
  if (!selected) return;
  const id = selected.id;
  const terminal = views.get(id);
  const running = active(id);
  const runId = db.active.find(item => item.id === id)?.runId;
  const currentRun = running && runId && chat.terminalRunId === runId;
  const prompt = currentRun ? chat.terminalPrompt : null;
  const ready = currentRun && !prompt && chat.ticket?.runId === runId;
  const terminalInputPending = terminal?.hasPendingInput();
  const needsLink = selected.provider === "codex" && selected.workspaceKind === "direct" && !chat.conversation;
  $("#chat-bind").hidden = running || !needsLink;
  $("#chat-start").hidden = running || needsLink;
  $("#chat-start").disabled = starting.has(id) || chat.applyingModel || chat.applyingApproval;
  $("#chat-send").disabled = !ready || terminalInputPending || !!chat.attaching || chat.sending || chat.applyingModel || chat.applyingApproval || (!chat.draft.trim() && !chat.images.length);
  const message = tr(chat.applyingModel ? "Model ayarı uygulanıyor…"
    : chat.sending ? "Gönderiliyor…"
    : chat.attaching ? "Dosya ekleniyor…"
    : prompt ? ""
    : chat.mismatch || needsLink ? "Bu konuşmayı Terminal görünümünde bağlayıp yeniden başlat."
    : !running ? "Mesaj göndermek için terminali başlat."
    : terminalInputPending ? "Terminal girdisi işleniyor…"
    : !ready ? "Sağlayıcı henüz yeni mesaj almaya hazır değil."
    : "");
  const status = $("#chat-status");
  if (status.textContent !== message) status.textContent = message;
  const title = $("#chat-terminal-prompt-title");
  const promptText = $("#chat-terminal-prompt-text");
  $("#chat-terminal-prompt").hidden = !prompt;
  const heading = prompt ? tr(prompt.kind === "approval" ? "Onayın bekleniyor" : "Yanıtın bekleniyor") : "";
  if (title.textContent !== heading) title.textContent = heading;
  const promptBody = prompt?.kind === "approval" && prompt.menu ? prompt.menu.question || prompt.text : prompt?.text || "";
  if (promptText.textContent !== promptBody) promptText.textContent = promptBody;
  const choiceArea = $("#chat-terminal-choices");
  const menu = prompt?.menu;
  const choiceKey = menu ? `${id}:${runId}:${menu.key}` : "";
  const questionCard = prompt?.kind === "question" && menu;
  choiceArea.classList.toggle("chat-choices", !questionCard);
  const renderKey = `${choiceKey}:${uiLanguage}`;
  if (choiceArea.dataset.key !== renderKey) {
    choiceArea.dataset.key = renderKey;
    choiceArea.replaceChildren();
    if (menu && questionCard) {
      choiceArea.append(createChatQuestion(chat, choiceKey, menu.question,
        menu.choices.map((label, index) => ({ number: index + 1, label, description: menu.descriptions?.[index] || "" }))
          .filter(choice => choice.number !== menu.customChoice),
        response => answerTerminalChoice(id, choiceKey, response), !!menu.customChoice));
    } else if (menu) {
      for (const [index, label] of menu.choices.entries()) {
        const button = el("button", "", `${index + 1} · ${label}`);
        button.type = "button";
        if (menu.descriptions?.[index]) button.title = menu.descriptions[index];
        button.onclick = () => answerTerminalChoice(id, choiceKey, { choice: index + 1 }).catch(error);
        choiceArea.append(button);
      }
    }
  }
  $("#chat-terminal-prompt").classList.toggle("question-card", !!questionCard);
  promptText.hidden = !!questionCard;
  choiceArea.hidden = !menu;
  updateQuestionControls(choiceArea, !!chat.answeringPrompt || !!terminalInputPending);
  for (const card of $("#chat-turns").querySelectorAll(".chat-question-card")) {
    updateQuestionControls(card, !ready || terminalInputPending || chat.sending || !!chat.pending || !!chat.loadError
      || !!chat.applyingModel || !!chat.applyingApproval || !!chat.attaching);
  }
  const peek = running && !ready && !prompt && !chat.mismatch;
  const expanded = peek && chat.terminalPreviewOpen === true;
  $("#chat-terminal-peek").hidden = !expanded;
  const toggle = $("#chat-show-terminal-output");
  toggle.hidden = !peek;
  toggle.setAttribute("aria-expanded", String(expanded));
  const outputLabel = tr(expanded ? "Canlı Terminal çıktısını gizle" : "Canlı Terminal çıktısını göster");
  toggle.title = outputLabel;
  toggle.setAttribute("aria-label", outputLabel);
  const output = $("#chat-terminal-output");
  output.hidden = !expanded;
  const liveText = expanded && currentRun ? chat.terminalOutput || "" : "";
  if (output.textContent !== liveText) output.textContent = liveText;
  paintChatModelControls(selected, chat);
  paintChatApproval(selected, chat);
}

async function answerTerminalChoice(id, key, response) {
  const chat = chatView(id);
  const bindingRevision = chat.bindingRevision;
  const terminal = views.get(id);
  const runId = db.active.find(item => item.id === id)?.runId;
  const menu = chat.terminalPrompt?.menu;
  if (shownChatId !== id || !runId || chat.answeringPrompt || !menu
    || terminal?.hasPendingInput() || chat.terminalRunId !== runId
    || key !== `${id}:${runId}:${menu.key}`) return false;
  chat.answeringPrompt = true;
  chat.promptRevision = (chat.promptRevision || 0) + 1;
  paintChatComposer(session(id), chat);
  try {
    await api("chat", "answer_prompt", {
      id, runId, key: menu.key, inputRevision: menu.inputRevision,
      selected: menu.selected, ...response,
    });
    if (chat.bindingRevision !== bindingRevision || chatViews.get(id) !== chat
      || db.active.find(item => item.id === id)?.runId !== runId) return false;
    if (chat.terminalPrompt?.menu?.key === menu.key) chat.terminalPrompt = null;
    return true;
  } finally {
    chat.promptRevision = (chat.promptRevision || 0) + 1;
    chat.answeringPrompt = false;
    if (shownChatId === id) {
      paintChatComposer(session(id), chat);
      readChat().catch(error);
      readChatInteraction().catch(error);
    }
  }
}

function questionOption(label, description = "") {
  const bold = label.match(/^\*\*(.+?)\*\*\s*[—–:.\-]?\s*(.*)$/);
  if (bold) return { label: bold[1], description: [bold[2], description].filter(Boolean).join(" ") };
  const parts = label.match(/^(.*?)\s+[—–]\s+([\s\S]*)$/);
  return { label: parts ? parts[1] : label, description: [parts?.[2], description].filter(Boolean).join(" ") };
}

function updateQuestionControls(card, disabled) {
  card.dataset.disabled = String(!!disabled);
  for (const nested of card.querySelectorAll(".chat-question-card")) nested.dataset.disabled = String(!!disabled);
  for (const control of card.querySelectorAll("input, button")) {
    control.disabled = !!disabled || (control.classList.contains("chat-custom-send")
      && !card.querySelector(".chat-custom-input")?.value.trim()) || (control.classList.contains("chat-choice-send")
      && !card.querySelector('input[type="radio"]:checked'));
  }
}

function createChatQuestion(view, key, question, choices, respond, custom = true) {
  const card = el("section", "chat-question-card");
  card.dataset.questionKey = key;
  const heading = el("div", "chat-question-heading");
  const mark = el("span", "chat-question-mark", "?");
  mark.setAttribute("aria-hidden", "true");
  heading.append(mark, el("strong", "", tr("Soru · Birini seç")));
  card.append(heading, el("p", "chat-question-title", question));
  const group = el("div", "chat-question-options");
  group.setAttribute("role", "radiogroup");
  group.setAttribute("aria-label", question);
  const submit = async response => {
    if (card.dataset.disabled === "true" || !card.isConnected) return;
    updateQuestionControls(card, true);
    try {
      if (await respond(response) !== true) return;
      if (view.questionReply?.key === key) view.questionReply = null;
      if (card.classList.contains("chat-reply-question")) view.answeredQuestionKey = key;
      card.remove();
      if (shownChatId && chatViews.get(shownChatId) === view) paintChatTurns(view);
    } catch (err) {
      error(err);
    } finally {
      if (shownChatId) paintChatComposer(session(shownChatId), chatView(shownChatId));
    }
  };
  for (const [index, choice] of choices.entries()) {
    const number = choice.number || index + 1;
    const option = questionOption(choice.label, choice.description);
    const row = el("label", "chat-question-option");
    const radio = el("input");
    radio.type = "radio";
    radio.name = "question-" + key;
    radio.value = String(number);
    radio.checked = view.questionReply?.key === key && view.questionReply.choice === number;
    radio.onchange = () => {
      view.questionReply = { key, text: card.querySelector(".chat-custom-input")?.value || "", choice: number };
      choiceSend.hidden = false;
      updateQuestionControls(card, card.dataset.disabled === "true");
    };
    const content = el("span", "chat-choice-content");
    content.append(el("span", "chat-choice-title", option.label));
    if (option.description) content.append(el("span", "chat-choice-description", option.description));
    row.append(radio, content);
    group.append(row);
  }
  card.append(group);
  const choiceSend = el("button", "chat-choice-send primary", tr("Seçimi gönder"));
  choiceSend.type = "button";
  choiceSend.hidden = !group.querySelector('input:checked');
  choiceSend.onclick = () => {
    const choice = Number(group.querySelector('input:checked')?.value);
    if (choice) submit({ choice });
  };
  card.append(choiceSend);
  if (custom) {
    const entry = el("div", "chat-custom-answer");
    const input = el("input", "chat-custom-input");
    input.type = "text";
    input.maxLength = 4000;
    input.placeholder = tr("Kendi yanıtını yaz…");
    input.setAttribute("aria-label", tr("Kendi yanıtın"));
    input.value = view.questionReply?.key === key ? view.questionReply.text : "";
    const button = el("button", "chat-custom-send", "↑");
    button.type = "button";
    button.setAttribute("aria-label", tr("Yanıtı gönder"));
    button.title = tr("Yanıtı gönder");
    input.oninput = () => {
      view.questionReply = { key, text: input.value };
      for (const radio of group.querySelectorAll("input")) radio.checked = false;
      choiceSend.hidden = true;
      button.disabled = card.dataset.disabled === "true" || !input.value.trim();
    };
    button.onclick = () => { if (input.value.trim()) submit({ text: input.value.trim() }); };
    input.onkeydown = event => {
      if (event.key === "Enter" && !event.isComposing) {
        event.preventDefault();
        event.stopPropagation();
        if (!button.disabled) button.click();
      }
    };
    entry.append(input, button);
    card.append(entry);
  }
  updateQuestionControls(card, true);
  return card;
}

const approvalLabels = {
  default: "Sağlayıcı varsayılanı", manual: "Onay iste", auto: "Benim için onayla",
  edits: "Düzenlemeleri onayla", plan: "Plan modu",
};

function approvalModes(provider) {
  return provider === "claude" ? ["default", "manual", "auto", "edits", "plan"]
    : ["default", "manual", "auto"];
}

function approvalOptions(provider) {
  return approvalModes(provider).map(mode => {
    const option = new Option(tr(approvalLabels[mode]), mode);
    option.disabled = provider === "copilot" && mode === "default";
    return option;
  });
}

function paintChatApproval(selected, chat) {
  const control = $("#chat-approval");
  if (control.dataset.provider !== selected.provider || control.dataset.language !== uiLanguage) {
    control.dataset.provider = selected.provider;
    control.dataset.language = uiLanguage;
    control.replaceChildren(...approvalOptions(selected.provider));
  }
  control.value = selected.provider === "copilot" && active(selected.id)
    ? chat.liveApproval || "default"
    : selected.approvalMode || "default";
  control.disabled = !!chat.applyingApproval || !!chat.sending || !!chat.applyingModel
    || (selected.provider === "copilot" && (!active(selected.id) || !chat.ticket));
  control.title = tr(selected.provider === "copilot" ? "Copilot girişe döndüğünde onay seviyesi değiştirilebilir."
    : "Onay seviyesi terminalin bir sonraki başlangıcında uygulanır.");
}

async function selectChatApproval(mode) {
  const selected = selectedChatSession();
  if (!selected || selected.id !== shownChatId) return;
  const chat = chatView(selected.id);
  const ticket = chat.ticket;
  chat.applyingApproval = true;
  paintChatComposer(selected, chat);
  try {
    const applied = await api("chat", "approval", { id: selected.id, mode, ...ticket });
    const current = session(selected.id);
    if (current) current.approvalMode = applied.approvalMode;
    if (selected.provider === "copilot") chat.liveApproval = applied.approvalMode;
    if (active(selected.id) && selected.provider !== "copilot")
      toast(tr("Onay seviyesi terminalin bir sonraki başlangıcında uygulanır."));
    await sync();
  } finally {
    chat.applyingApproval = false;
    if (selected.provider === "copilot") chat.ticket = null;
    if (shownChatId === selected.id) {
      paintChatComposer(session(selected.id), chat);
      scheduleChatRead();
      scheduleChatInteraction();
    }
  }
}

async function chooseSessionImages(id) {
  const picked = await window.__TAURI__.core.invoke("plugin:dialog|open", {
    options: { multiple: true },
  });
  const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
  return attachmentReferences(id, paths);
}

function imagePrompt(images, provider) {
  const references = images.map(image => image.reference).join(" ");
  return !references ? "" : provider === "copilot" ? references : tr(
    images.every(attachment => attachment.kind === "image") ? "Görsel dosyaları {files}" : "Ekli dosyalar {files}",
    { files: references },
  );
}

function attachmentReferences(id, paths) {
  return Promise.all(paths.map(path => api("chat", "file_reference", { id, path })));
}

function discardScreenshot(id, attachment) {
  if (attachment?.attachments) return Promise.all(attachment.attachments.map(item => discardScreenshot(id, item)));
  if (attachment?.attachmentId) return api("chat", "discard_screenshot", { id, attachmentId: attachment.attachmentId });
}

async function collectChatAttachments(id, read) {
  const view = chatViews.get(id);
  if (!view) return;
  const bound = session(id)?.conversationId;
  const bindingRevision = view.bindingRevision;
  const stillBound = () => chatViews.get(id) === view && !!session(id)
    && view.bindingRevision === bindingRevision
    && (!bound || session(id).conversationId === bound);
  view.attaching = (view.attaching || 0) + 1;
  if (shownChatId === id) paintChatComposer(session(id), view);
  try {
    const attachments = (await read(stillBound)).flatMap(item => item?.attachments || [item]);
    if (!stillBound()) {
      await Promise.all(attachments.map(attachment => discardScreenshot(id, attachment)));
      return;
    }
    view.images = [...new Map([...view.images, ...attachments].map(attachment => [attachment.reference, attachment])).values()];
    if (shownChatId === id) paintChatImages(view);
  } finally {
    view.attaching--;
    if (shownChatId === id) paintChatComposer(session(id), chatViews.get(id) || view);
  }
}

async function saveClipboardImage(id, file) {
  if (file?.type !== "image/png") return api("clipboard", "read_attachment", { id });
  if (file.size > 40 * 1024 * 1024) throw Error(tr("Ekran görüntüsü çok büyük."));
  const data = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result).split(",", 2)[1]);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
  return api("chat", "paste_screenshot", { id, png: data });
}

function paintChatImages(view) {
  const list = $("#chat-images");
  list.hidden = !view.images.length;
  list.replaceChildren(...view.images.map(image => button(`${image.attachmentId ? tr("Ekran görüntüsü") : image.name} ×`, async () => {
    const id = shownChatId;
    view.images = view.images.filter(item => item !== image);
    paintChatImages(view);
    paintChatComposer(session(id), view);
    // An in-flight send may already refer to this file.
    if (!view.sending) await discardScreenshot(id, image);
  }, "chat-image-chip", tr("Eki kaldır"))));
}

function initAttachmentDrop() {
  let highlighted;
  const destination = (position) => {
    if (!position) return null;
    const node = document.elementFromPoint(position.x / devicePixelRatio, position.y / devicePixelRatio);
    const chat = node?.closest("#chat-view");
    if (chat && shownChatId) return { element: chat, id: shownChatId };
    const terminal = node?.closest(".session-card .terminal-host");
    const id = terminal?.closest(".session-card")?.dataset.id;
    const view = id && views.get(id);
    if (!view?.ready || view.replaying || !active(id) || session(id)?.login) return null;
    return { element: terminal, id, terminal: view };
  };
  const mark = (next) => {
    if (highlighted === next) return;
    highlighted?.classList.remove("file-drop-ready");
    highlighted = next;
    highlighted?.classList.add("file-drop-ready");
  };
  const hover = ({ payload }) => mark(destination(payload?.position)?.element);
  listen("tauri://drag-enter", hover).catch(error);
  listen("tauri://drag-over", hover).catch(error);
  listen("tauri://drag-leave", () => mark(null)).catch(error);
  listen("tauri://drag-drop", ({ payload }) => {
    const target = destination(payload.position);
    mark(null);
    if (!target || !payload.paths?.length) return;
    if (target.terminal) target.terminal.attachFiles(payload.paths);
    else collectChatAttachments(target.id, () => attachmentReferences(target.id, payload.paths)).catch(error);
  }).catch(error);
}

const reasoningLabels = {
  none: "Düşünme kapalı", minimal: "En düşük", low: "Düşük", medium: "Orta",
  high: "Yüksek", xhigh: "Çok yüksek", max: "Maksimum", ultra: "Ultra",
};

function chatModelSettings(selected, chat) {
  const terminal = active(selected.id) ? views.get(selected.id) : null;
  const find = (name) => name && chat.models?.find(model =>
    [model.id, model.name].some(value => value?.toLowerCase() === name.toLowerCase()));
  const saved = find(selected.model);
  const observed = find(terminal?.observedModel);
  const defaultModel = !selected.model && !terminal?.observedModel && chat.models?.find(model => model.default);
  return {
    model: observed || saved || defaultModel,
    modelLabel: observed?.name || terminal?.observedModel || saved?.name || selected.model || defaultModel?.name,
    effort: terminal?.observedEffort || selected.effort,
  };
}

function paintChatModelControls(selected, chat) {
  const { modelLabel, effort } = chatModelSettings(selected, chat);
  for (const [selector, label, value] of [
    ["#chat-model", "Model seç", modelLabel],
    ["#chat-effort", "Düşünme seviyesi", effort && tr(reasoningLabels[effort] || effort)],
  ]) {
    const control = $(selector);
    control.hidden = selector === "#chat-effort" && !["codex", "claude", "copilot"].includes(selected.provider);
    const text = value || tr(label);
    if (control.firstElementChild.textContent !== text) control.firstElementChild.textContent = text;
    const accessible = value ? `${tr(label)} · ${value}` : tr(label);
    control.setAttribute("aria-label", accessible);
    control.title = accessible;
    control.disabled = !!chat.applyingModel || !!chat.applyingApproval || !!chat.sending || starting.has(selected.id);
  }
  const menu = $("#chat-model-menu");
  const notice = menu.querySelector(".chat-model-blocked");
  if (notice && menu.matches(":popover-open")) {
    const blocked = chatModelBlocked(selected.id, chat);
    if (notice.textContent !== blocked) notice.textContent = blocked;
    notice.hidden = !blocked;
    for (const option of menu.querySelectorAll("button")) option.disabled = !!blocked;
  }
}

function chatModelBlocked(id, chat) {
  if (!active(id)) return "";
  const terminal = views.get(id);
  if (terminal?.hasPendingInput()) return tr("Terminal girdisi işleniyor…");
  if (!terminal?.ready || terminal.replaying || !chat.ticket || chat.ticket.runId !== terminal.runId)
    return tr("Model ayarı için Codex’in girişe dönmesini bekle.");
  return "";
}

async function applyChatModel(id, model, effort) {
  const selected = session(id);
  if (!selected || shownChatId !== id) return;
  const chat = chatView(id);
  const blocked = chatModelBlocked(id, chat);
  if (blocked || chat.sending || chat.applyingModel || starting.has(id)) {
    if (blocked) error(blocked);
    return;
  }
  $("#chat-model-menu").hidePopover();
  chat.applyingModel = true;
  paintChatComposer(selected, chat);
  try {
    const ticket = active(id) ? chat.ticket : null;
    const updated = await api("models", "apply", { id, surface: "chat", model: model.id, effort, ...ticket });
    Object.assign(session(id) || selected, { model: updated.model, effort: updated.effort });
    const terminal = views.get(id);
    if (ticket && terminal?.runId === ticket.runId) {
      terminal.observedModel = model.name || model.id;
      terminal.observedEffort = effort;
    }
    await sync();
  } catch (err) {
    error(err);
  } finally {
    chat.applyingModel = false;
    chat.ticket = null;
    if (shownChatId === id) {
      paintChatComposer(session(id), chat);
      scheduleChatRead();
    }
  }
}

let chatModelRequest;

async function openChatModelMenu(anchor, effortPicker) {
  const selected = session(shownChatId);
  if (!selected) return;
  if (selected.provider !== "codex") return openModels(selected.id);
  const menu = $("#chat-model-menu");
  if (menu.matches(":popover-open") && chatModelRequest?.anchor === anchor) {
    menu.hidePopover();
    return;
  }
  menu.hidePopover();
  const request = { anchor, id: selected.id };
  chatModelRequest = request;
  const chat = chatView(selected.id);
  menu.setAttribute("aria-label", anchor.title);
  menu.replaceChildren(el("p", "", tr("Yükleniyor…")));
  const rect = anchor.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(rect.left, innerWidth - 262))}px`;
  menu.style.bottom = `${innerHeight - rect.top + 6}px`;
  menu.style.maxHeight = `${Math.max(0, Math.min(280, rect.top - 14))}px`;
  menu.showPopover({ source: anchor });
  anchor.setAttribute("aria-expanded", "true");
  const stillOpen = () => chatModelRequest === request && shownChatId === request.id && menu.matches(":popover-open");
  try {
    chat.models = await api("models", "list", { id: request.id });
    if (!stillOpen()) return;
    const { model, effort } = chatModelSettings(session(request.id), chat);
    const item = (label, chosen, run) => {
      const option = button("", () => chosen && active(request.id) ? menu.hidePopover() : run());
      option.setAttribute("aria-pressed", String(chosen));
      option.append(el("span", "", label));
      if (chosen) {
        const marker = el("span", "", "✓");
        marker.setAttribute("aria-hidden", "true");
        option.append(marker);
      }
      return option;
    };
    const items = effortPicker ? (model?.efforts || []).map(({ reasoningEffort: level }) =>
      item(tr(reasoningLabels[level] || level), (effort || model.defaultEffort) === level, () => applyChatModel(request.id, model, level)))
      : chat.models.map(choice => {
        const same = choice.id === model?.id;
        const level = same && choice.efforts?.some(option => option.reasoningEffort === effort)
          ? effort : choice.defaultEffort;
        return item(choice.name || choice.id, same, () => applyChatModel(request.id, choice, level || null));
      });
    menu.replaceChildren(...items);
    if (!items.length) menu.append(el("p", "", tr(model ? "Bu model için düşünme seçeneği yok." : "Önce bir model seç.")));
    menu.append(el("p", "chat-model-blocked"));
    paintChatModelControls(session(request.id), chat);
    if (document.activeElement === anchor) menu.querySelector("button:not(:disabled)")?.focus();
  } catch (err) {
    if (stillOpen()) menu.replaceChildren(el("p", "", tr(err?.message || String(err))));
  }
}

function reconcileChatPrompt(view) {
  const pending = view.pending;
  const last = view.turns.at(-1);
  if (pending && last?.prompt === pending.text.trim() && last.position > pending.position) {
    view.pending = null;
    return true;
  }
  return false;
}

function paintChatTurns(view, initial = false) {
  const scroll = $("#chat-scroll");
  const turns = $("#chat-turns");
  const opened = new Set(initial ? [] : [...turns.querySelectorAll("details[open]")].map(node => node.dataset.turn));
  const follow = initial || scroll.scrollHeight - scroll.clientHeight - scroll.scrollTop < 55;
  const draw = (entry, index) => {
    const turn = el("article", "chat-turn");
    turn.append(el("div", "chat-prompt", entry.prompt));
    if (entry.steps?.length) {
      const details = el("details", "chat-steps");
      details.dataset.turn = String(entry.position ?? index);
      details.open = opened.has(details.dataset.turn);
      details.append(el("summary", "", countLabel(entry.steps.length, "{count} işlem")));
      const list = el("ol", "chat-step-list");
      for (const step of entry.steps) {
        const item = el("li");
        if (step.kind === "commentary") item.append(renderChatText(step.text));
        else item.append(
          el("span", "chat-tool-kind", tr({ command: "Komut", edit: "Dosya düzenleme", tool: "Araç" }[step.kind])),
          el("code", "", step.text),
        );
        list.append(item);
      }
      details.append(list);
      turn.append(details);
    }
    if (entry.answer) {
      const answer = el("div", "chat-answer");
      const provider = session(shownChatId)?.provider;
      const questionKey = `${shownChatId}:${view.conversation}:${entry.position}:${entry.answer}`;
      const question = !view.pending && view.answeredQuestionKey !== questionKey
        && entry === view.turns.at(-1) && entry.state === "completed"
        ? chatReplyQuestion(entry.answer) : null;
      answer.append(el("strong", "chat-answer-name", `${icons[provider]} ${names[provider]}`));
      if (!question || question.before) answer.append(renderChatText(question ? question.before : entry.answer));
      if (question) {
        const id = shownChatId;
        const key = questionKey;
        const card = createChatQuestion(view, key, question.question, question.choices, response => {
          const selected = selectedChatSession();
          const latest = view.turns.at(-1);
          if (selected?.id !== id || shownChatId !== id || chatViews.get(id) !== view
            || latest?.position !== entry.position || latest?.answer !== entry.answer
            || view.loadError || view.sending || view.pending) throw Error(tr("Soru değişti. Güncel seçeneklerden birini seç."));
          return sendChat(selected, view, response.text || String(response.choice));
        });
        card.classList.add("chat-reply-question");
        answer.append(card);
      }
      turn.append(answer);
    } else if (entry.state === "interrupted") {
      turn.append(el("p", "help", tr("Yanıt kesildi.")));
    } else if (entry.state === "working") {
      turn.append(el("p", "help", tr(active(shownChatId) ? "Yanıt bekleniyor…" : "Bu istek için yanıt kaydedilmedi.")));
    }
    return turn;
  };
  turns.replaceChildren(...view.turns.map(draw));
  if (view.pending) turns.append(draw({ prompt: view.pending.text, state: "working" }, view.turns.length));
  paintChatEmpty(view);
  if (follow) scroll.scrollTop = scroll.scrollHeight;
}

function chatReplyQuestion(answer) {
  const lines = answer.split(/\r?\n/);
  const matchChoice = line => line.match(/^([1-9])(?:[.)]\s+|\s+[-–—]\s+)(.+)$/);
  // Only offer choices from an explicit question at the end of the latest reply.
  // Numbered instructions and code samples remain ordinary message text.
  if (lines.some(line => /^\s*(```|~~~)/.test(line))) return null;
  while (lines.length && !lines.at(-1).trim()) lines.pop();
  const last = lines.at(-1);
  const questionAfter = last && !/^\s/.test(last) && !matchChoice(last) && last.endsWith("?") ? lines.pop() : null;
  while (lines.length && !lines.at(-1).trim()) lines.pop();
  const start = lines.findLastIndex(line => matchChoice(line)?.[1] === "1");
  if (start < 0) return null;
  const choices = [];
  let gap = false;
  for (const line of lines.slice(start)) {
    const choice = matchChoice(line);
    if (choice) {
      if (Number(choice[1]) !== choices.length + 1 || !choice[2].trim()) return null;
      choices.push({ label: choice[2], description: "" });
      gap = false;
    } else if (line.trim()) {
      const current = choices.at(-1);
      if (!current || gap || !/^[ \t]+\S/.test(line)) return null;
      current.description = [current.description, line.trim()].filter(Boolean).join(" ");
    } else gap = true;
  }
  const before = lines.slice(0, start);
  while (before.length && !before.at(-1).trim()) before.pop();
  const question = questionAfter || (before.at(-1)?.trim().endsWith("?") ? before.pop().trim() : null);
  const asksForChoice = question && /\b(?:which|choose|select|prefer|would you|do you want|should (?:i|we)|how should)\b|hangisi|hangileri|hangi |neyi |neleri |seç|tercih|ister misin|nasıl (?:ilerle|yap|devam)/i.test(question);
  return choices.length >= 2 && asksForChoice ? { question, choices, before: before.join("\n").trimEnd() } : null;
}

function renderChatText(text) {
  const output = el("div", "chat-rich");
  const lines = text.split(/\r?\n/);
  const appendInline = (parent, line) => {
    for (const part of line.split(/(\x60[^\x60]+\x60|\*\*[^*]+\*\*)/g)) {
      if (part.startsWith("\x60") && part.endsWith("\x60") && part.length > 2)
        parent.append(el("code", "", part.slice(1, -1)));
      else if (part.startsWith("**") && part.endsWith("**") && part.length > 4)
        parent.append(el("strong", "", part.slice(2, -2)));
      else parent.append(part);
    }
  };
  let paragraph = null, list = null, code = null;
  for (const line of lines) {
    if (/^\s*\x60{3}/.test(line)) {
      if (code) code = null;
      else {
        code = el("code");
        const pre = el("pre");
        pre.append(code);
        output.append(pre);
      }
      paragraph = list = null;
    } else if (code) code.append(line + "\n");
    else if (!line.trim()) paragraph = list = null;
    else {
      const item = line.match(/^\s*([-*]|\d+[.)])\s+(.+)/);
      const heading = line.match(/^\s*#{1,6}\s+(.+)/);
      if (item) {
        paragraph = null;
        const tag = /^\d/.test(item[1]) ? "OL" : "UL";
        if (list?.tagName !== tag) {
          list = el(tag.toLowerCase());
          output.append(list);
        }
        const row = el("li");
        appendInline(row, item[2]);
        list.append(row);
      } else if (heading) {
        paragraph = list = null;
        const title = el("h3");
        appendInline(title, heading[1]);
        output.append(title);
      } else {
        list = null;
        if (paragraph) paragraph.append(" ");
        else {
          paragraph = el("p");
          output.append(paragraph);
        }
        appendInline(paragraph, line);
      }
    }
  }
  return output;
}

async function sendChatMessage(event) {
  event.preventDefault();
  const selected = selectedChatSession();
  if (!selected || selected.id !== shownChatId || $("#chat-send").disabled) return;
  const view = chatView(selected.id);
  const draft = view.draft;
  const images = view.images;
  const text = [draft.trim(), imagePrompt(images, selected.provider)].filter(Boolean).join("\n\n");
  await sendChat(selected, view, text, { draft, images });
}

async function sendChat(selected, view, text, composer) {
  const id = selected.id;
  const bindingRevision = view.bindingRevision;
  const pending = { text, position: view.turns.at(-1)?.position ?? -1 };
  const latestQuestion = view.turns.at(-1);
  const answeredQuestionKey = !view.pending && latestQuestion?.state === "completed" && typeof latestQuestion.answer === "string"
    && chatReplyQuestion(latestQuestion.answer)
    ? `${id}:${view.conversation}:${latestQuestion.position}:${latestQuestion.answer}` : null;
  view.sending = true;
  paintChatComposer(selected, view);
  try {
    await api("chat", "send", { id, ...view.ticket, text });
    if (view.bindingRevision !== bindingRevision) return false;
    if (answeredQuestionKey) view.answeredQuestionKey = answeredQuestionKey;
    view.pending = pending;
    reconcileChatPrompt(view);
    if (composer && view.draft === composer.draft) {
      view.draft = "";
      if (shownChatId === id) $("#chat-input").value = "";
    }
    if (composer) {
      view.images = view.images.filter(image => !composer.images.includes(image));
      if (shownChatId === id) paintChatImages(view);
    }
    if (shownChatId === id) paintChatTurns(view);
    return true;
  } finally {
    view.ticket = null;
    view.sending = false;
    if (shownChatId === id) {
      paintChatComposer(session(id), view);
      scheduleChatRead();
    }
  }
}

function initChatControls() {
  $("#session-terminal-view").onclick = () => setChatView(false);
  $("#session-chat-view").onclick = () => setChatView(true);
  $("#chat-answer-terminal").onclick = () => {
    const id = shownChatId;
    if (!id) return;
    setChatView(false);
    requestAnimationFrame(() => {
      if (!shownChatId && focusId === id) views.get(id)?.term.focus();
    });
  };
  $("#chat-show-terminal-output").onclick = () => {
    const id = shownChatId;
    if (!id) return;
    const view = chatView(id);
    view.terminalPreviewOpen = !view.terminalPreviewOpen;
    paintChatComposer(session(id), view);
    if (view.terminalPreviewOpen) readChatInteraction().catch(error);
  };
  $("#chat-approval").onchange = (event) => selectChatApproval(event.target.value).catch(error);
  $("#chat-attach-image").onclick = async () => {
    const id = shownChatId;
    if (!id) return;
    try {
      await collectChatAttachments(id, () => chooseSessionImages(id));
    } catch (err) { error(err); }
  };
  $("#chat-model").onclick = (event) => openChatModelMenu(event.currentTarget, false).catch(error);
  $("#chat-effort").onclick = (event) => openChatModelMenu(event.currentTarget, true).catch(error);
  $("#chat-model-menu").addEventListener("beforetoggle", (event) => {
    if (event.newState === "closed") {
      chatModelRequest?.anchor.setAttribute("aria-expanded", "false");
      chatModelRequest = undefined;
    }
  });
  $("#chat-bind").onclick = () => {
    if (shownChatId && !active(shownChatId)) bindHistory(shownChatId).catch(error);
  };
  $("#chat-start").onclick = () => {
    if (shownChatId && !active(shownChatId)) toggleTerminal(shownChatId).catch(error);
  };
  $("#chat-composer").onsubmit = event => sendChatMessage(event).catch(error);
  initAttachmentDrop();
  $("#chat-input").addEventListener("paste", (event) => {
    const image = [...(event.clipboardData?.files || [])].find(file => file.type.startsWith("image/"));
    const id = shownChatId;
    if (!id || !image) return;
    event.preventDefault();
    collectChatAttachments(id, async () => {
      const attachment = await saveClipboardImage(id, image);
      return attachment ? [attachment] : [];
    }).catch(error);
  });
  $("#chat-input").oninput = (event) => {
    if (!shownChatId) return;
    const view = chatView(shownChatId);
    view.draft = event.target.value;
    paintChatComposer(session(shownChatId), view);
  };
  $("#chat-input").onkeydown = (event) => {
    if ((event.ctrlKey || event.metaKey) && !event.shiftKey && !event.altKey && event.key.toLowerCase() === "v" && shownChatId) {
      event.preventDefault();
      const id = shownChatId;
      const input = event.currentTarget;
      collectChatAttachments(id, async (stillBound) => {
        const attachment = await api("clipboard", "read_attachment", { id });
        if (attachment) return [attachment];
        const text = await api("clipboard", "read");
        if (text && shownChatId === id && stillBound()) {
          input.setRangeText(text, input.selectionStart, input.selectionEnd, "end");
          input.dispatchEvent(new Event("input", { bubbles: true }));
        }
        return [];
      }).catch(error);
      return;
    }
    if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    $("#chat-composer").requestSubmit();
  };
}
