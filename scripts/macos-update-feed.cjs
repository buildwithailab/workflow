"use strict";

// Builds only the public Mac feed. The protected signing key is never used here.
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const manifestName = "Workflow-Update-macOS-arm64.json";
const packageName = "Workflow-Update-macOS-arm64.app.zip";
const repository = "buildwithailab/workflow";
const maxFeed = 272 * 1024;
const maxPackage = 128 * 1024 * 1024;

function requireValue(condition, message) { if (!condition) throw Error(message); }
function stableVersion(text) {
  if (typeof text !== "string" || !/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.test(text)) return null;
  const numbers = text.split(".").map(BigInt);
  return numbers.every(number => number <= 18446744073709551615n) ? numbers : null;
}
function compare(a, b) {
  const left = stableVersion(a), right = stableVersion(b);
  requireValue(left && right, "Invalid stable version");
  for (let i = 0; i < 3; i++) { if (left[i] !== right[i]) return left[i] > right[i] ? 1 : -1; }
  return 0;
}
function boundedFile(file, maximum) {
  const info = fs.lstatSync(file);
  requireValue(info.isFile() && !info.isSymbolicLink() && info.size <= maximum, "Invalid input file");
  const data = fs.readFileSync(file);
  requireValue(data.length <= maximum && data.length === info.size, "Input size changed");
  return data;
}
function decode(text, maximum) {
  requireValue(typeof text === "string" && text.length <= Math.ceil(maximum / 3) * 4, "Invalid encoded field");
  const bytes = Buffer.from(text, "base64");
  requireValue(bytes.length <= maximum && bytes.toString("base64") === text, "Invalid base64 field");
  return bytes;
}
function exactKeys(value, keys) {
  requireValue(value && typeof value === "object" && !Array.isArray(value) &&
    Object.keys(value).sort().join("\0") === keys.slice().sort().join("\0"), "Unexpected manifest fields");
}
function verifyManifest(bytes, publicText) {
  requireValue(bytes.length <= maxFeed, "Update metadata is too large");
  const envelope = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  exactKeys(envelope, ["algorithm", "payload", "signature"]);
  requireValue(envelope.algorithm === "ecdsa-p256-sha256", "Unsupported update algorithm");
  const payload = decode(envelope.payload, 192 * 1024), signature = decode(envelope.signature, 64);
  const publicBytes = decode(publicText.trim(), 72);
  requireValue(signature.length === 64 && publicBytes.length === 72 &&
    publicBytes.subarray(0, 8).equals(Buffer.from([69, 67, 83, 49, 32, 0, 0, 0])), "Invalid public key or signature");
  const publicKey = crypto.createPublicKey({ format: "jwk", key: { kty: "EC", crv: "P-256",
    x: publicBytes.subarray(8, 40).toString("base64url"), y: publicBytes.subarray(40, 72).toString("base64url") } });
  requireValue(crypto.verify("sha256", payload, { key: publicKey, dsaEncoding: "ieee-p1363" }, signature), "Update signature is invalid");
  const value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(payload));
  exactKeys(value, ["schema", "repository", "channel", "version", "asset", "size", "sha256", "notes"]);
  requireValue(value.schema === 1 && value.repository === repository && value.channel === "stable" &&
    stableVersion(value.version) && value.asset === packageName && Number.isSafeInteger(value.size) &&
    value.size >= 64 && value.size <= maxPackage && /^[a-fA-F0-9]{64}$/.test(value.sha256) &&
    typeof value.notes === "string" && [...value.notes].length <= 40000 && !/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]/.test(value.notes), "Invalid Mac release payload");
  return value;
}
function prepare(manifest, archive, publicText, tag, previous) {
  const value = verifyManifest(manifest, publicText);
  requireValue(tag === "v" + value.version + "-macos", "Release tag does not match signed version");
  requireValue(archive.length === value.size && archive.subarray(0, 4).equals(Buffer.from([80, 75, 3, 4])) &&
    crypto.createHash("sha256").update(archive).digest("hex") === value.sha256.toLowerCase(), "Package does not match signed metadata");
  if (previous) {
    const old = verifyManifest(previous, publicText);
    requireValue(compare(value.version, old.version) >= 0, "Mac channel downgrade refused");
    requireValue(compare(value.version, old.version) !== 0 ||
      (old.sha256.toLowerCase() === value.sha256.toLowerCase() && old.size === value.size && old.notes === value.notes), "A published version cannot be replaced");
  }
  return value;
}
function selectRelease(releases) {
  requireValue(Array.isArray(releases), "Invalid release list");
  releases = releases.flat();
  const candidates = releases.filter(r => r && !r.draft && !r.prerelease && r.immutable === true &&
    typeof r.tag_name === "string" && /^v.+-macos$/.test(r.tag_name) && stableVersion(r.tag_name.slice(1, -6)));
  requireValue(candidates.length, "No immutable stable Mac release found");
  candidates.sort((a, b) => compare(b.tag_name.slice(1, -6), a.tag_name.slice(1, -6)));
  return candidates[0].tag_name;
}

if (require.main === module) {
  try {
    if (process.argv[2] === "select" && process.argv.length === 4) {
      console.log(selectRelease(JSON.parse(boundedFile(process.argv[3], 16 * 1024 * 1024))));
    } else {
      const [manifestPath, packagePath, tag, output, previousPath] = process.argv.slice(2);
      requireValue(manifestPath && packagePath && tag && output && process.argv.length <= 7, "Use manifest package tag output [previous-manifest]");
      const manifest = boundedFile(manifestPath, maxFeed);
      const publicText = boundedFile(path.join(__dirname, "../src/update-public-key.txt"), 1024).toString("utf8");
      const value = prepare(manifest, boundedFile(packagePath, maxPackage), publicText, tag,
        previousPath ? boundedFile(previousPath, maxFeed) : null);
      requireValue(!fs.existsSync(output), "Output already exists");
      fs.mkdirSync(path.join(output, "updates"), { recursive: true });
      fs.writeFileSync(path.join(output, "updates", manifestName), manifest, { flag: "wx" });
      fs.writeFileSync(path.join(output, ".nojekyll"), "", { flag: "wx" });
      console.log("Verified Mac update channel prepared for " + value.version + "; no Windows feed written.");
    }
  } catch { console.error("Mac update channel validation failed; nothing was published."); process.exitCode = 1; }
}
module.exports = { prepare, verifyManifest, selectRelease, compare };
