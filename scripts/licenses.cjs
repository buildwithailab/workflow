const fs = require("fs"),
  path = require("path"),
  cp = require("child_process");
const root = path.resolve(__dirname, "..");
const options = process.argv.slice(2);
if (options.length % 2 || options.some((value, index) => index % 2 === 0 && !["--target", "--output"].includes(value)))
  throw Error("Use --target <triple> and --output <file>");
const option = (name, fallback) => options.includes(name) ? options[options.indexOf(name) + 1] : fallback;
const selectedTarget = option("--target", null);
const targets = ["x86_64-pc-windows-msvc", "aarch64-apple-darwin"];
if (selectedTarget && !targets.includes(selectedTarget)) throw Error("Unsupported notice target");
const fallbackLicenses = JSON.parse(fs.readFileSync(path.join(__dirname, "licenses", "sources.json"), "utf8"));
// One repository distributes both platforms. Keep one combined notice file so
// packaging on either host cannot silently omit the other platform's licenses.
const packages = new Map();
for (const target of selectedTarget ? [selectedTarget] : targets) {
  const metadata = JSON.parse(cp.execFileSync("cargo", [
    "metadata", "--format-version", "1", "--locked", "--filter-platform", target,
  ], {
    cwd: root, encoding: "utf8", maxBuffer: 16 * 1024 * 1024, windowsHide: true,
  }));
  for (const item of metadata.packages) packages.set(item.id, item);
}
const sections = [
  "Workflow third-party notices\n\nThe application includes the libraries listed below. Their source code remains subject to the corresponding licenses.",
];
for (const p of [...packages.values()].sort((a, b) =>
  a.name.localeCompare(b.name) || a.version.localeCompare(b.version),
)) {
  if (p.name === "workflow") continue;
  const folder = path.dirname(p.manifest_path);
  const candidates = fs
    .readdirSync(folder)
    .filter((n) => /^(licen[cs]e|copying|notice)([._-]|$)/i.test(n));
  if (p.license_file) {
    const declared = path.relative(folder, path.resolve(folder, p.license_file));
    if (!declared || declared.startsWith("..") || path.isAbsolute(declared))
      throw Error("License file escapes package: " + p.name);
    if (!candidates.includes(declared)) candidates.push(declared);
  }
  const texts = [];
  for (const n of candidates) {
    const file = path.join(folder, n);
    if (fs.statSync(file).isFile() && fs.statSync(file).size < 100000)
      texts.push(n + "\n" + fs.readFileSync(file, "utf8"));
  }
  if (!texts.length) {
    const fallback = fallbackLicenses.find(item => item.packages.includes(p.name + "@" + p.version));
    if (!fallback || fallback.repository !== p.repository || !/^[a-z0-9.-]+\.txt$/i.test(fallback.file))
      throw Error("Missing reviewed license text: " + p.name + "@" + p.version);
    texts.push("Upstream license: " + fallback.source + "\n" +
      fs.readFileSync(path.join(__dirname, "licenses", fallback.file), "utf8"));
  }
  sections.push(
    "\n" +
      "=".repeat(72) +
      "\n" +
      p.name +
      " " +
      p.version +
      "\nLicense: " +
      (p.license || "See included license") +
      "\nSource: " +
      (p.repository || p.source || "crates.io") +
      "\n\n" +
      texts.join("\n\n"),
  );
}
fs.writeFileSync(
  path.resolve(root, option("--output", "THIRD-PARTY-NOTICES.txt")),
  sections.join("\n").replace(/\r\n/g, "\n").replace(/[\t ]+$/gm, ""),
);
console.log("Dependency notices:", packages.size - 1);
