"use strict";
// Fail closed. Reports contain only file paths, line numbers and rule names.
const fs = require("node:fs"), path = require("node:path"), cp = require("node:child_process");

const blockedDirectories = /^(?:target|dist|node_modules|test-results|test-input|test-output|test-data|playwright-report|blob-report|coverage|__pycache__|accounts|profiles|sessions|transcripts|leases|secrets|\.secrets|ui-profile|browser-profiles|.+-profile|test-.+-home|\.aws|\.azure|\.kube|\.ssh|\.codex|\.claude|\.copilot|\.workflow|\.agentloom|local\.workflow\.desktop|local\.agentloom\.desktop)$/i;
const blockedNames = /^(?:\.env.*|.*\.env|\.npmrc|\.netrc|(?:\.)?credentials(?:\..*)?\.json|auth(?:-status)?\.json|secrets(?:\..*)?\.json|workspace\.json|ready\.json|id_rsa.*|id_ed25519.*|Cookies|Login Data|Local State|Web Data|History)$/i;
const blockedExtensions = /\.(?:exe|dll|pdb|obj|lib|zip|7z|rar|tar|gz|log|ansi|pem|key|pfx|p12|der|keystore|jks|kdbx|db(?:-wal|-shm)?|sqlite[^/]*|dmp|har|pyc)$/i;
const assetPath = /^(?:icons\/[^/]+\.(?:png|ico)|docs\/images\/[^/]+\.(?:png|jpe?g|gif|webp))$/i;
const maxSize = 12 * 1024 * 1024;
const rules = [
  ["private-key", /-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED )?PRIVATE KEY-----/g],
  ["provider-token", /\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{30,}|sk-(?:proj-|ant-[A-Za-z0-9]+-)?[A-Za-z0-9_-]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|AIza[A-Za-z0-9_-]{35}|(?:AKIA|ASIA)[A-Z0-9]{16})\b/g],
  ["jwt", /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g],
  ["credential-in-url", /\b(?:https?|postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis):\/\/[^\s/:'"<>]+:[^\s/@'"<>]{3,}@/gi],
  ["literal-secret", /["']?(?:api[_-]?key|client[_-]?secret|access[_-]?token|refresh[_-]?token|secret[_-]?key|password|passwd|private[_-]?key|signing[_-]?key|token|secret)["']?\s*[:=]\s*["']([A-Za-z0-9_+/.=:@-]+)["']/gi],
  ["dotenv-secret", /\b[A-Z][A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|API_KEY|PRIVATE_KEY)\s*=\s*[A-Za-z0-9_+/.=-]{8,}/g],
  ["connection-password", /\b(?:Password|Pwd|AccountKey|SharedAccessKey)\s*=\s*[A-Za-z0-9_+/.=-]{8,}(?=[;\s"'])/gi],
  ["authorization-literal", /["'](?:Bearer|Basic) [A-Za-z0-9_+/.=-]{16,}["']/g],
  ["personal-home-path", /(?:[A-Z]:[\\/]+(?:Users|Documents and Settings)[\\/]+[^\\/\s"'<>]+[\\/]|\/(?:home|Users)\/[^/\s"'<>]+\/)/g],
];

function pathRule(name) {
  const parts = name.replace(/\\/g, "/").split("/");
  if (parts.some(part => blockedDirectories.test(part))) return "runtime-or-generated-data";
  const file = parts.at(-1);
  if (blockedNames.test(file) || blockedExtensions.test(file) || /\.local\.json$/i.test(file)) return "secret-or-local-file";
  if (/(?:^|\/)(?:\.cargo\/(?:config(?:\.toml)?|credentials[^/]*)|\.docker\/config\.json|gen\/schemas\/)/i.test(parts.join("/"))) return "local-or-generated-config";
  if (/\.(?:jsonl|ndjson|json|csv|txt|sql|bin)$/i.test(file) && /(?:^|\/)(?:fixtures|captures|screenshots|recordings|dumps)(?:\/|$)/i.test(name)) return "recorded-test-data";
  return null;
}

function scanText(name, text) {
  const findings = [];
  for (const [rule, regex] of rules) {
    regex.lastIndex = 0;
    for (const match of text.matchAll(regex)) {
      findings.push({ file: name, line: text.slice(0, match.index).split("\n").length, rule });
    }
  }
  return findings;
}

function scanFile(name, bytes, changedBinary = false) {
  const blocked = pathRule(name);
  if (blocked) return [{ file: name, rule: blocked }];
  if (bytes.length > maxSize) return [{ file: name, rule: "too-large-to-audit" }];
  let text;
  try {
    if (bytes[0] === 0xff && bytes[1] === 0xfe) text = bytes.subarray(2).toString("utf16le");
    else text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch { text = null; }
  if (text !== null && !text.includes("\0")) return scanText(name, text);
  const findings = scanText(name, bytes.toString("latin1"));
  findings.push(...scanText(name, bytes.toString("utf16le")), ...scanText(name, bytes.subarray(1).toString("utf16le")));
  if (!assetPath.test(name)) findings.push({ file: name, rule: "unreviewable-binary" });
  else if (changedBinary) findings.push({ file: name, rule: "changed-binary-needs-explicit-review" });
  return findings;
}

function git(root, args) {
  const result = cp.spawnSync("git", ["-C", root, ...args], { windowsHide: true, encoding: null, maxBuffer: 64 * 1024 * 1024 });
  if (result.error || result.status !== 0) throw Error("Git operation failed; audit blocked (" + args[0] + ").");
  return result.stdout;
}

function auditIndex(root) {
  const repository = git(root, ["rev-parse", "--show-toplevel"]).toString("utf8").trim();
  if (fs.realpathSync(root) !== fs.realpathSync(repository)) {
    throw Error("Run from the Git repository root; audit blocked.");
  }
  root = repository;
  const changed = new Set(git(root, ["diff", "--cached", "--name-only", "--diff-filter=ACMRT", "-z"]).toString("utf8").split("\0"));
  const records = git(root, ["ls-files", "--stage", "-z"]).toString("utf8").split("\0").filter(Boolean);
  if (!records.length) throw Error("Empty or unavailable index; audit blocked.");
  const findings = [];
  for (const record of records) {
    const entry = /^(\d+) ([a-f0-9]+) ([0-3])\t([\s\S]+)$/.exec(record);
    if (!entry) throw Error("Could not parse the index; audit blocked.");
    const [, mode, oid, stage, name] = entry;
    if (stage !== "0" || !["100644", "100755"].includes(mode)) {
      findings.push({ file: name, rule: "unmerged-link-or-submodule" });
      continue;
    }
    const blocked = pathRule(name);
    if (blocked) { findings.push({ file: name, rule: blocked }); continue; }
    const size = Number(git(root, ["cat-file", "-s", oid]).toString("utf8").trim());
    if (!Number.isSafeInteger(size) || size > maxSize) { findings.push({ file: name, rule: "too-large-to-audit" }); continue; }
    findings.push(...scanFile(name, git(root, ["cat-file", "blob", oid]), changed.has(name)));
  }
  return { files: records.length, findings, excluded: [], media: [] };
}

function auditWorktree(root) {
  const findings = [], excluded = [], media = [];
  let files = 0;
  function walk(directory, relative = "") {
    for (const item of fs.readdirSync(directory, { withFileTypes: true })) {
      const name = relative + item.name, absolute = path.join(directory, item.name);
      if (item.name === ".git") continue;
      if (item.isSymbolicLink()) { findings.push({ file: name, rule: "unreviewable-link" }); continue; }
      if (pathRule(name + (item.isDirectory() ? "/" : ""))) { excluded.push(name); continue; }
      if (item.isDirectory()) walk(absolute, name + "/");
      else if (item.isFile()) {
        files++;
        if (assetPath.test(name)) media.push(name);
        const size = fs.statSync(absolute).size;
        if (size > maxSize) findings.push({ file: name, rule: "too-large-to-audit" });
        else findings.push(...scanFile(name, fs.readFileSync(absolute)));
      } else findings.push({ file: name, rule: "unreviewable-file" });
    }
  }
  walk(root);
  return { files, findings, excluded, media };
}

function main() {
  const mode = process.argv[2];
  if (process.argv.length !== 3 || !["--staged", "--worktree"].includes(mode)) throw Error("Use --staged before a commit, or --worktree for a source-only audit.");
  const result = mode === "--staged" ? auditIndex(process.cwd()) : auditWorktree(process.cwd());
  for (const finding of result.findings) console.error(`${JSON.stringify(finding.file)}${finding.line ? ":" + finding.line : ""}: BLOCKED ${finding.rule}`);
  if (result.findings.length) {
    console.error(`BLOCKED: ${result.findings.length} finding(s); ${result.files} files checked. No values were printed.`);
    process.exitCode = 1;
  } else console.log(`PASS: ${result.files} files checked for prohibited paths and secret patterns.`);
  if (mode === "--worktree") {
    console.log("SOURCE-ONLY AUDIT: does not verify the Git index or history, and does not authorize a commit.");
    console.log("Excluded local/generated paths: " + result.excluded.map(name => JSON.stringify(name)).join(", "));
    if (result.media.length) console.log("Media still requires visual review (including animation frames): " + result.media.map(name => JSON.stringify(name)).join(", "));
  }
}

module.exports = { pathRule, scanText, scanFile, auditIndex, auditWorktree, main };
if (require.main === module) {
  try { main(); } catch {
    console.error("BLOCKED: audit could not complete. Run in the repository root with a readable Git index; no commit is permitted.");
    process.exitCode = 1;
  }
}
