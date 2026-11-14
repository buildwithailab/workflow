async function teamsPage({ action, args }) {
  if (!["teams.microsoft.com", "teams.cloud.microsoft"].includes(location.hostname))
    return null;

  const editor = () => document.querySelector('[data-tid="ckeditor"][role="textbox"][contenteditable="true"]');
  const title = () => document.querySelector('[data-tid="chat-title"]');
  const normalize = (text) => text.replace(/\r\n?/g, "\n").replace(/\u00a0/g, " ").trim();

  function targetIsOpen(id) {
    return !!editor() && title()?.closest('[id^="chat-header-"]')?.id === `chat-header-${id}`;
  }

  function sendButton(box) {
    const container = box.closest('[data-tid="message-pane-layout"]') || document;
    const buttons = container.querySelectorAll('button[data-tid="sendMessageCommands-send"]');
    const button = buttons.length === 1 ? buttons[0] : null;
    return button && !button.disabled && button.getAttribute("aria-disabled") !== "true" ? button : null;
  }

  if (action === "status") {
    return {
      ready: !!document.querySelector('[data-tid="simple-collab-dnd-rail"]') || !!(title() && editor()),
      document: performance.timeOrigin,
    };
  }
  if (action === "open-target") {
    if (targetIsOpen(args.id)) return true;
    const key = CSS.escape(args.id);
    const rows = document.querySelectorAll(
      `[role="treeitem"][data-fui-tree-item-value="${key}"],` +
      `[role="treeitem"][data-fui-tree-item-value$="|${key}"]`
    );
    if (rows.length !== 1) return false;
    rows[0].click();
    return true;
  }
  if (action === "prepare") {
    if (location.pathname === "/dl/launcher/launcher.html") {
      const launcher = document.querySelector("button#openTeamsClientInBrowser");
      launcher?.click();
      return { target: false };
    }
    return { target: performance.timeOrigin !== args.previousDocument && targetIsOpen(args.id) };
  }
  if (action === "compose") {
    if (!targetIsOpen(args.id)) return { state: "target" };
    const box = editor();
    if (normalize(box.innerText)) return { state: "draft" };
    box.focus({ preventScroll: true });
    const range = document.createRange();
    range.selectNodeContents(box.querySelector("p") || box);
    range.collapse(true);
    const selection = window.getSelection();
    if (!selection) return { state: "unsent" };
    selection.removeAllRanges();
    selection.addRange(range);
    const clipboard = new DataTransfer();
    clipboard.setData("text/plain", args.text);
    box.dispatchEvent(new ClipboardEvent("paste", {
      clipboardData: clipboard,
      bubbles: true,
      cancelable: true,
      composed: true,
    }));
    await new Promise(resolve => setTimeout(resolve, 350));
    if (!targetIsOpen(args.id) || editor() !== box) return { state: "target" };
    return { state: normalize(box.innerText) ? "composed" : "empty" };
  }
  if (action === "submit") {
    if (!targetIsOpen(args.id)) return { state: "target" };
    const box = editor();
    const button = sendButton(box);
    if (!button || normalize(box.innerText) !== normalize(args.text)) return { state: "unsent" };
    button.click();
    return { state: "submitted" };
  }
  return false;
}
