"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs"), path = require("node:path"), cp = require("node:child_process");
const { randomBytes } = require("node:crypto");
const { pathRule, scanText, scanFile, auditIndex, auditWorktree } = require("../scripts/check-commit.cjs");
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

test("Mac metadata and forced application resources never pass the staged gate", () => {
  for (const name of [".DS_Store", "nested/._sample.txt", ".AppleDouble/a", "Workflow.app/Contents/Info.plist", "debug/Workflow.dSYM/Contents/Info.plist", "Archive.xcarchive/Info.plist", "Workflow.dmg", "nested/Workflow.pkg"])
    assert(pathRule(name), name);
  assert.equal(pathRule("tauri.macos.conf.json"), null);
  const directory = repository();
  fs.writeFileSync(path.join(directory, ".gitignore"), "*.app/\n");
  const resource = "Workflow.app/Contents/Info.plist";
  fs.mkdirSync(path.dirname(path.join(directory, resource)), { recursive: true });
  fs.writeFileSync(path.join(directory, resource), "synthetic package metadata\n");
  git(directory, ["add", "-f", "--", resource]);
  assert(auditIndex(directory).findings.some(f => f.file === resource && f.rule === "platform-metadata-or-build-product"));
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
const macIcons = ["icons/icon-macos.png", "icons/icon-macos@2x.png"];
function iconRepository() {
  const directory = repository();
  fs.mkdirSync(path.join(directory, "icons"));
  for (const name of macIcons)
    fs.copyFileSync(path.join(root, name), path.join(directory, name));
  git(directory, ["add", "--", ...macIcons]);
  return directory;
}
test("only the two exact reviewed paths, byte sequences and regular index modes pass", () => {
  const directory = iconRepository();
  assert.deepEqual(auditIndex(directory).findings, []);
  const [name, retina] = macIcons;
  const original = fs.readFileSync(path.join(root, name));
  const different = Buffer.from(original);
  different[different.length - 1] ^= 1;
  const cases = [
    different, original.subarray(0, 32), Buffer.concat([original, Buffer.from([0])]),
    fs.readFileSync(path.join(root, retina)), Buffer.from("plain text replacement"),
    Buffer.from("version https://git-lfs.github.com/spec/v1\noid sha256:" + "0".repeat(64) + "\nsize 1\n"),
  ];
  for (const bytes of cases) {
    fs.writeFileSync(path.join(directory, name), bytes);
    git(directory, ["add", "--", name]);
    assert(auditIndex(directory).findings.some(f => f.file === name && f.rule === "reviewed-icon-content-mismatch"));
  }
  for (const other of ["icons/third.png", "icons/Icon-macos.png", "docs/images/icon-macos.png"])
    assert(scanFile(other, original, true, "100644").some(f => f.rule === "changed-binary-needs-explicit-review"));
  fs.copyFileSync(path.join(root, name), path.join(directory, "icons", "third.png"));
  git(directory, ["add", "--", "icons/third.png"]);
  assert(auditIndex(directory).findings.some(f => f.file === "icons/third.png" && f.rule === "changed-binary-needs-explicit-review"));
});
test("reviewed icon checks use the index blob and preserve secret redaction", () => {
  const directory = iconRepository(), name = macIcons[0], file = path.join(directory, name);
  const original = fs.readFileSync(file);
  const value = syntheticToken();
  fs.writeFileSync(file, Buffer.concat([original, Buffer.from("\n" + value + "\n")]));
  assert.deepEqual(auditIndex(directory).findings, []);
  assert(auditWorktree(directory).findings.some(f => f.rule === "reviewed-icon-content-mismatch"));
  git(directory, ["add", "--", name]);
  fs.writeFileSync(file, original);
  const findings = auditIndex(directory).findings;
  for (const rule of ["reviewed-icon-content-mismatch", "changed-binary-needs-explicit-review", "provider-token"])
    assert(findings.some(f => f.file === name && f.rule === rule), rule);
  assert(!JSON.stringify(findings).includes(value));
});
test("reviewed icon approval does not permit an executable or symbolic link index entry", () => {
  const directory = iconRepository(), name = macIcons[1];
  git(directory, ["update-index", "--chmod=+x", "--", name]);
  assert(auditIndex(directory).findings.some(f => f.file === name && f.rule === "reviewed-icon-mode-mismatch"));
  git(directory, ["update-index", "--chmod=-x", "--", name]);
  assert.deepEqual(auditIndex(directory).findings, []);
  const oid = git(directory, ["hash-object", "-w", "--", "safe.txt"]).trim();
  git(directory, ["update-index", "--cacheinfo", "120000," + oid + "," + name]);
  assert(auditIndex(directory).findings.some(f => f.file === name && f.rule === "unmerged-link-or-submodule"));
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
