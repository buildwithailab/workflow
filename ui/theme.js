"use strict";

// Shared by the application and its notification window. Never touches provider input.
(() => {
  const key = "workflow-theme", choices = new Set(["dark", "light", "system"]);
  const media = window.matchMedia("(prefers-color-scheme: light)");
  const terminals = new Set();
  let preference = "dark";
  try {
    const saved = localStorage.getItem(key);
    if (choices.has(saved)) preference = saved;
  } catch { /* Storage may be disabled; the theme still works for this window. */ }

  function terminalTheme() {
    const style = getComputedStyle(document.documentElement);
    const names = ["background", "foreground", "cursor", "cursorAccent", "selectionBackground",
      "selectionInactiveBackground", "black", "red", "green", "yellow", "blue", "magenta",
      "cyan", "white", "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue",
      "brightMagenta", "brightCyan", "brightWhite"];
    return Object.fromEntries(names.map(name => [name,
      style.getPropertyValue("--terminal-" + name.replace(/[A-Z]/g, c => "-" + c.toLowerCase())).trim()]));
  }
  function apply() {
    const root = document.documentElement;
    root.dataset.theme = preference === "system" ? (media.matches ? "light" : "dark") : preference;
    root.dataset.themePreference = preference;
    if (terminals.size) {
      const palette = terminalTheme();
      for (const terminal of terminals) updateTerminal(terminal, palette);
    }
    window.dispatchEvent(new Event("workflow-theme-change"));
  }
  function updateTerminal(terminal, palette) {
    terminal.options.theme = { ...palette };
    terminal.options.minimumContrastRatio = document.documentElement.dataset.theme === "light" ? 4.5 : 1;
  }
  function setPreference(value) {
    if (!choices.has(value)) throw new TypeError("Invalid theme preference");
    preference = value;
    let persisted = true;
    try { localStorage.setItem(key, value); } catch { persisted = false; }
    apply();
    return persisted;
  }
  window.addEventListener("storage", event => {
    if (event.key !== key && event.key !== null) return;
    preference = choices.has(event.newValue) ? event.newValue : "dark";
    apply();
  });
  media.addEventListener("change", () => { if (preference === "system") apply(); });
  window.workflowTheme = Object.freeze({
    getPreference: () => preference,
    setPreference,
    terminalTheme,
    bindTerminal(terminal) {
      updateTerminal(terminal, terminalTheme());
      terminals.add(terminal);
      return () => terminals.delete(terminal);
    },
  });
  apply();
})();
