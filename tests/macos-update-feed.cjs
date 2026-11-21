"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const { prepare, verifyManifest, selectRelease } = require("../scripts/macos-update-feed.cjs");

function fixture(version = "1.2.3", patch = {}) {
  const { privateKey, publicKey } = crypto.generateKeyPairSync("ec", { namedCurve: "P-256" });
  const point = publicKey.export({ format: "jwk" });
  const publicText = Buffer.concat([Buffer.from([69,67,83,49,32,0,0,0]),
    Buffer.from(point.x, "base64url"), Buffer.from(point.y, "base64url")]).toString("base64");
  const archive = Buffer.concat([Buffer.from([80,75,3,4]), crypto.randomBytes(124)]);
  const value = { schema: 1, repository: "buildwithailab/workflow", channel: "stable", version,
    asset: "Workflow-Update-macOS-arm64.app.zip", size: archive.length,
    sha256: crypto.createHash("sha256").update(archive).digest("hex"), notes: "Synthetic update", ...patch };
  const sign = value => {
    const bytes = Buffer.from(JSON.stringify(value));
    return Buffer.from(JSON.stringify({ algorithm: "ecdsa-p256-sha256", payload: bytes.toString("base64"),
      signature: crypto.sign("sha256", bytes, { key: privateKey, dsaEncoding: "ieee-p1363" }).toString("base64") }));
  };
  return { manifest: sign(value), archive, publicText, value, sign };
}
test("Mac feed verifies signature and exact package; other platforms and tags are rejected", () => {
  const f = fixture();
  assert.equal(prepare(f.manifest, f.archive, f.publicText, "v1.2.3-macos").version, "1.2.3");
  assert.throws(() => prepare(f.manifest, f.archive, f.publicText, "v1.2.3"));
  assert.throws(() => prepare(f.manifest, f.archive.subarray(1), f.publicText, "v1.2.3-macos"));
  assert.throws(() => verifyManifest(f.manifest, fixture().publicText));
  for (const patch of [{ asset: "Workflow-Update-Windows-x64.exe" }, { channel: "preview" },
    { repository: "example/other" }, { version: "01.2.3" }, { version: "1.2.3-rc" }, { size: 1 }, { notes: "\u0000" }]) {
    assert.throws(() => verifyManifest(f.sign({ ...f.value, ...patch }), f.publicText));
  }
  const altered = JSON.parse(f.manifest);
  altered.payload = Buffer.from(JSON.stringify({ ...f.value, version: "9.9.9" })).toString("base64");
  assert.throws(() => verifyManifest(Buffer.from(JSON.stringify(altered)), f.publicText));
});
test("Mac channel refuses rollback and replacement of the same published version", () => {
  const f = fixture();
  const old = f.sign({ ...f.value, version: "1.2.2" });
  assert.equal(prepare(f.manifest, f.archive, f.publicText, "v1.2.3-macos", old).version, "1.2.3");
  assert.throws(() => prepare(f.manifest, f.archive, f.publicText, "v1.2.3-macos", f.sign({ ...f.value, version: "1.2.4" })));
  assert.throws(() => prepare(f.manifest, f.archive, f.publicText, "v1.2.3-macos", f.sign({ ...f.value, notes: "Other content" })));
});
test("channel selection ignores Windows, previews, drafts and mutable releases", () => {
  const stable = tag_name => ({ tag_name, draft: false, prerelease: false, immutable: true });
  const rows = [stable("v99.0.0"), stable("v1.2.9-macos"), stable("v1.2.10-macos"),
    { ...stable("v2.0.0-macos"), prerelease: true }, { ...stable("v3.0.0-macos"), draft: true },
    { ...stable("v4.0.0-macos"), immutable: false }, stable("v5.0.0-macos-preview.1")];
  assert.equal(selectRelease([rows]), "v1.2.10-macos");
  assert.throws(() => selectRelease([stable("v1.2.3")]));
});
