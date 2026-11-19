"use strict";

const copy = {
  en: {
    approval: "A session is waiting for approval.",
    question: "A session is waiting for your answer.",
    completed: "Work finished in a session.",
    open: "Open session", result: "View result", dismiss: "Dismiss notification",
  },
  tr: {
    approval: "Bir oturum onay bekliyor.",
    question: "Bir oturum yanıt bekliyor.",
    completed: "Bir oturumda iş tamamlandı.",
    open: "Oturuma git", result: "Sonucu gör", dismiss: "Bildirimi kapat",
  },
};

const card = document.querySelector("#card");
const openButton = document.querySelector("#open");
const closeButton = document.querySelector("#dismiss");
let currentId = null;
let revision = 0;

function popup(action, id) {
  return window.__TAURI__.core.invoke("dispatch", {
    domain: "notification_popup", action, args: id ? { id } : {},
  });
}

async function refresh() {
  const latest = ++revision;
  const entry = await popup("read");
  if (revision !== latest) return;
  currentId = entry?.id || null;
  const valid = !!currentId && ["approval", "question", "completed"].includes(entry.kind);
  card.querySelector(".state").hidden = !valid;
  card.querySelector("footer").hidden = !valid;
  openButton.disabled = closeButton.disabled = !valid;
  if (!valid) return;

  const language = entry.turkish ? "tr" : "en";
  const words = copy[language];
  document.documentElement.lang = language;
  card.dataset.kind = entry.kind;
  document.querySelector("#symbol").textContent = entry.kind === "completed" ? "✓" : entry.kind === "question" ? "?" : "!";
  document.querySelector("#message").textContent = words[entry.kind];
  openButton.textContent = entry.kind === "completed" ? words.result : words.open;
  closeButton.setAttribute("aria-label", words.dismiss);
  closeButton.title = words.dismiss;
  await popup("presented", currentId);
}

window.refreshWorkflowPopup = () => refresh().catch(() => {});

async function act(action) {
  if (!currentId || openButton.disabled) return;
  const id = currentId;
  openButton.disabled = closeButton.disabled = true;
  try {
    await popup(action, id);
  } catch {
    if (currentId === id) window.refreshWorkflowPopup();
  }
}

openButton.addEventListener("click", () => act("open"));
closeButton.addEventListener("click", () => act("dismiss"));
document.addEventListener("keydown", event => {
  if (event.key === "Escape") act("dismiss");
});
document.addEventListener("contextmenu", event => event.preventDefault());
window.refreshWorkflowPopup();
