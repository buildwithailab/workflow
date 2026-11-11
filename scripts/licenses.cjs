const fs = require("fs"),
  path = require("path"),
  cp = require("child_process");
const root = path.resolve(__dirname, "..");
const metadata = JSON.parse(
  cp.execFileSync(
    "cargo",
    [
      "metadata",
      "--format-version",
      "1",
      "--locked",
      "--filter-platform",
      "x86_64-pc-windows-msvc",
    ],
    {
      cwd: root,
      encoding: "utf8",
      maxBuffer: 16 * 1024 * 1024,
      windowsHide: true,
    },
  ),
);
const sections = [
  "Workflow third-party notices\n\nThe application includes the libraries listed below. Their source code remains subject to the corresponding licenses.",
];
for (const p of metadata.packages.sort((a, b) =>
  a.name.localeCompare(b.name),
)) {
  if (p.name === "workflow") continue;
  const folder = path.dirname(p.manifest_path);
  const candidates = fs
    .readdirSync(folder)
    .filter((n) => /^(licen[cs]e|copying|notice)([._-]|$)/i.test(n));
  const texts = [];
  for (const n of candidates) {
    const file = path.join(folder, n);
    if (fs.statSync(file).isFile() && fs.statSync(file).size < 100000)
      texts.push(n + "\n" + fs.readFileSync(file, "utf8"));
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
  path.join(root, "THIRD-PARTY-NOTICES.txt"),
  sections.join("\n"),
);
console.log("Dependency notices:", metadata.packages.length - 1);
