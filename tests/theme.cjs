"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const vm = require("node:vm");
const fs = require("node:fs");
const path = require("node:path");
const source = fs.readFileSync(path.join(__dirname, "../ui/theme.js"), "utf8");
const css = fs.readFileSync(path.join(__dirname, "../ui/theme.css"), "utf8");

function fixture(saved, osLight = false, denied = false) {
  const listeners = new Map(), media = { matches: osLight, addEventListener(type, fn) { this.change = fn; } };
  const root = { dataset: {} }, writes = [];
  const window = { matchMedia: () => media,
    addEventListener(type, fn) { listeners.set(type, fn); }, dispatchEvent() {} };
  const localStorage = {
    getItem() { if (denied) throw Error("storage unavailable"); return saved; },
    setItem(key, value) { if (denied) throw Error("storage unavailable"); writes.push([key, value]); },
  };
  vm.runInNewContext(source, { window, document: { documentElement: root }, localStorage, Event,
    getComputedStyle: () => ({ getPropertyValue: name => {
      const section = root.dataset.theme === "light" ? css.split(':root[data-theme="light"]')[1] : css.split(':root[data-theme="light"]')[0];
      return section.match(new RegExp(name + ": ([^;]+);"))?.[1] || "";
    } }),
  });
  return { theme: window.workflowTheme, root, writes,
    os(value) { media.matches = value; media.change(); }, storage(value, key = "workflow-theme") { listeners.get("storage")({ key, newValue: value }); } };
}
test("theme defaults, validated persistence and blocked storage", () => {
  for (const saved of [undefined, "invalid"]) assert.equal(fixture(saved, true).root.dataset.theme, "dark");
  assert.equal(fixture("light").root.dataset.theme, "light");
  const f = fixture("light", false, true);
  assert.equal(f.theme.setPreference("light"), false);
  assert.equal(f.root.dataset.theme, "light");
  assert.throws(() => f.theme.setPreference("other"), /Invalid theme/);
});
test("system follows the OS, manual choice stays fixed, cross-window updates do not echo", () => {
  const f = fixture("system", true);
  assert.equal(f.root.dataset.theme, "light");
  f.os(false); assert.equal(f.root.dataset.theme, "dark");
  f.theme.setPreference("light"); f.os(false); assert.equal(f.root.dataset.theme, "light");
  f.storage("dark"); assert.equal(f.root.dataset.theme, "dark");
  f.storage("light", "unrelated"); assert.equal(f.root.dataset.theme, "dark");
  assert.equal(f.writes.length, 1);
  f.storage(null); assert.equal(f.theme.getPreference(), "dark");
});
test("switching only changes terminal palettes and cleanup releases disposed views", () => {
  const f = fixture("dark");
  const terminals = ["codex", "claude", "copilot"].map(() => ({ options: {}, write() { assert.fail("theme sent input"); }, reset() { assert.fail("theme reset terminal"); } }));
  const release = terminals.map(t => f.theme.bindTerminal(t));
  f.theme.setPreference("light");
  for (const t of terminals) {
    assert.equal(t.options.theme.background, "#ffffff");
    assert.equal(Object.keys(t.options.theme).length, 22);
    assert(Object.values(t.options.theme).every(value => /^#[0-9a-f]{6}$/.test(value)));
  }
  const old = terminals[0].options.theme;
  release[0](); f.theme.setPreference("dark");
  assert.equal(terminals[0].options.theme, old);
  assert.equal(terminals[1].options.theme.background, "#0c1015");
});

function luminance(hex) {
  const channels = hex.match(/[a-f\d]{2}/gi).map(value => parseInt(value, 16) / 255).map(value => value <= .04045 ? value / 12.92 : ((value + .055) / 1.055) ** 2.4);
  return channels[0] * .2126 + channels[1] * .7152 + channels[2] * .0722;
}
test("both palettes provide readable text, controls, diffs and syntax", () => {
  for (const section of css.split(/:root(?:\[data-theme="light"\])? \{/).slice(1)) {
    const tokens = Object.fromEntries([...section.matchAll(/--([\w-]+): (#[\da-f]{6});/g)].map(m => [m[1], m[2]]));
    for (const [fg, bg] of [["text", "bg"], ["muted", "panel"], ["on-accent", "accent"], ["added-code", "added-surface"], ["removed-code", "removed-surface"], ["permission-ink", "question-surface"], ["json-key", "json-surface"], ["json-string", "json-surface"], ["amber", "warning-surface"]]) {
      if (!tokens[fg] || !tokens[bg]) continue;
      const a = luminance(tokens[fg]), b = luminance(tokens[bg]);
      assert((Math.max(a, b) + .05) / (Math.min(a, b) + .05) >= 4.5, fg + " on " + bg);
    }
  }
});
