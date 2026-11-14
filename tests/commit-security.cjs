"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs"), path = require("node:path"), cp = require("node:child_process");
const { randomBytes } = require("node:crypto");
const { pathRule, scanText, scanFile, auditIndex } = require("../scripts/check-commit.cjs");
const root = path.resolve(__dirname, "..");
const generated = path.join(root, "target");
const syntheticToken = () => ["gh", "p", "_", randomBytes(20).toString("hex")].join("");
function git(directory, args) {
  return cp.execFileSync("git", ["-C", directory, ...args], { windowsHide: true, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}
function repository() {
  fs.mkdirSync(generated, { recursive: true });
  // Disposable fixture only. Never initialize or commit in the project itself.
  const directory = fs.mkdtempSync(path.join(generated, "security-gate-test-"));
  git(directory, ["init", "--quiet"]);
  fs.writeFileSync(path.join(directory, "safe.txt"), "synthetic source\n");
  git(directory, ["add", "safe.txt"]);
  return directory;
}
test("blocks runtime, environment, account and generated test files at any depth", () => {
  for (const name of [".env", ".env.example", "nested/.env.production", "local.env", "accounts/a/auth.json", "nested/sessions/a.json", "transcripts/a.ansi", "test-input/provider.txt", "target/fixture/data.json", "dist/release.zip", "test-results/capture.png", "browser-profiles/p/Cookies", "ui-profile/Preferences", "workspace.json", "database.sqlite-wal", "nested/.cargo/credentials.toml", "key.pfx", "fixtures/recorded.json"]) {
    assert(pathRule(name), name);
  }
  for (const name of ["src/terminal.rs", "tests/provider-fixture.rs", "tests/terminal-input.cjs", "Cargo.lock", ".gitignore", "AGENTS.md"]) assert.equal(pathRule(name), null, name);
});
test("findings never include the secret value", () => {
  const value = syntheticToken();
  const findings = scanText("src/example.js", "\nconst value = " + JSON.stringify(value));
  assert(findings.some(f => f.rule === "provider-token" && f.line === 2));
  assert(!JSON.stringify(findings).includes(value));
});
test("detects private keys, quoted secrets, env assignments and credential URLs", () => {
  const samples = [
    ["-----BEGIN ", "PRIVATE KEY", "-----"].join(""),
    ["api", "_key = ", JSON.stringify(randomBytes(16).toString("hex"))].join(""),
    ["TEST_", "SECRET=", randomBytes(16).toString("hex")].join(""),
    ["https://fixture", ":", randomBytes(16).toString("hex"), "@example.com"].join(""),
  ];
  for (const text of samples) assert(scanText("src/example.js", text).length > 0);
  const utf16 = Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(samples[1], "utf16le")]);
  assert(scanFile("source.txt", utf16).some(f => f.rule === "literal-secret"));
});
test("unknown binaries and changed public images block the commit", () => {
  const bytes = Buffer.from([0, 137, 80, 78, 71]);
  assert(scanFile("opaque.bin", bytes).some(f => f.rule === "unreviewable-binary"));
  assert(scanFile("docs/images/demo.png", bytes, true).some(f => f.rule === "changed-binary-needs-explicit-review"));
});
test("checks staged contents even when the working file has been cleaned", () => {
  const directory = repository(), value = syntheticToken();
  fs.writeFileSync(path.join(directory, "source.txt"), value);
  git(directory, ["add", "source.txt"]);
  fs.writeFileSync(path.join(directory, "source.txt"), "clean working copy");
  const report = auditIndex(directory);
  assert(report.findings.some(f => f.file === "source.txt" && f.rule === "provider-token"));
  assert(!JSON.stringify(report).includes(value));
});
test("force-adding an ignored file is still blocked", () => {
  const directory = repository();
  fs.copyFileSync(path.join(root, ".gitignore"), path.join(directory, ".gitignore"));
  fs.writeFileSync(path.join(directory, ".env"), "synthetic local settings");
  git(directory, ["add", ".gitignore"]);
  git(directory, ["add", "--force", ".env"]);
  assert(auditIndex(directory).findings.some(f => f.file === ".env"));
});
test("valid staged source passes; absence of Git fails closed", () => {
  assert.deepEqual(auditIndex(repository()).findings, []);
  const empty = fs.mkdtempSync(path.join(generated, "security-no-index-"));
  assert.throws(() => auditIndex(empty), /audit blocked/);
});
test("hook uses the indexed scanner, and cannot pass with a missing scanner", () => {
  const directory = repository();
  const shell = process.platform === "win32" ? path.join(process.env.ProgramFiles, "Git", "bin", "sh.exe") : "sh";
  function hook() { return cp.spawnSync(shell, [path.join(root, ".githooks/pre-commit")], { cwd: directory, windowsHide: true, encoding: "utf8" }); }
  let result = hook();
  assert.equal(result.status, 1, result.stderr);
  fs.mkdirSync(path.join(directory, "scripts"));
  fs.copyFileSync(path.join(root, "scripts/check-commit.cjs"), path.join(directory, "scripts/check-commit.cjs"));
  git(directory, ["add", "scripts/check-commit.cjs"]);
  result = hook();
  assert.equal(result.status, 0, result.stderr);
  fs.writeFileSync(path.join(directory, "scripts/check-commit.cjs"), "process.exit(0)");
  fs.writeFileSync(path.join(directory, "source.txt"), syntheticToken());
  git(directory, ["add", "source.txt"]);
  result = hook();
  assert.equal(result.status, 1, "unstaged scanner changes cannot weaken the gate");
});
