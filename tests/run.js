#!/usr/bin/env node
// Regression tests for src/editor.js — the ~700-line canvas annotation editor
// that has no automated coverage otherwise (see CLAUDE.md's headless-Chrome
// harness pattern).
//
// There is no app server and no build step to test against: this script
// copies the real editor files into a temp dir, wires a stubbed Tauri bridge
// and a test driver around them (see tests/harness/), and drives the whole
// thing with headless Chrome via --dump-dom. That's the only way to exercise
// real canvas pointer events and hit-testing without a GUI.
//
// Usage: node tests/run.js   (or: npm test)

const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const REPO_ROOT = path.resolve(__dirname, "..");
const SRC = path.join(REPO_ROOT, "src");
const CHROME =
  process.env.RECAP_TEST_CHROME ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

function fail(msg) {
  console.error(`\n${msg}`);
  process.exit(1);
}

for (const f of ["editor.js", "editor.css", "editor.html", "styles.css"]) {
  if (!fs.existsSync(path.join(SRC, f))) fail(`missing source file: src/${f}`);
}
if (!fs.existsSync(CHROME)) {
  fail(
    `Chrome not found at ${CHROME}. Set RECAP_TEST_CHROME to your Chrome binary.`
  );
}

// ---- stage a temp copy of the editor + harness ------------------------------

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "recap-editor-test-"));

for (const f of ["editor.js", "editor.css", "styles.css"]) {
  fs.copyFileSync(path.join(SRC, f), path.join(tmp, f));
}
fs.copyFileSync(path.join(__dirname, "harness", "stub.js"), path.join(tmp, "harness-stub.js"));
fs.copyFileSync(path.join(__dirname, "harness", "driver.js"), path.join(tmp, "harness-driver.js"));

const editorHtml = fs.readFileSync(path.join(SRC, "editor.html"), "utf8");
const scriptTag = '<script src="editor.js"></script>';
if (!editorHtml.includes(scriptTag)) {
  fail(`editor.html no longer contains ${JSON.stringify(scriptTag)} — update tests/run.js to match`);
}
const testHtml = editorHtml.replace(
  scriptTag,
  `<script src="harness-stub.js"></script>\n  ${scriptTag}\n  <script src="harness-driver.js"></script>`
);
fs.writeFileSync(path.join(tmp, "test.html"), testHtml);

// ---- drive it with headless Chrome ------------------------------------------

const url = `file://${path.join(tmp, "test.html")}?path=test.png`;
const args = [
  "--headless=new",
  "--disable-gpu",
  "--hide-scrollbars",
  "--window-size=1600,1200",
  "--virtual-time-budget=10000", // gives the driver's async waitFor room to settle
  "--dump-dom",
  url,
];

const run = spawnSync(CHROME, args, {
  encoding: "utf8",
  maxBuffer: 32 * 1024 * 1024,
  timeout: 60000,
});

if (run.error) fail(`failed to launch Chrome: ${run.error.message}`);

const dom = run.stdout || "";
const match = dom.match(
  /<script type="application\/json" id="recap-test-results">([\s\S]*?)<\/script>/
);

if (!match) {
  console.error("--- chrome stderr ---");
  console.error(run.stderr);
  console.error("--- dumped DOM (truncated) ---");
  console.error(dom.slice(0, 4000));
  fail("test harness did not produce a results block — see output above (chrome exit code " + run.status + ")");
}

let data;
try {
  data = JSON.parse(match[1]);
} catch (e) {
  fail(`results block was not valid JSON: ${e.message}\n${match[1].slice(0, 2000)}`);
}

// ---- report -------------------------------------------------------------------

for (const r of data.results) {
  if (r.pass) {
    console.log(`PASS  ${r.name}`);
  } else {
    console.log(`FAIL  ${r.name}`);
    console.log(`      ${r.error.split("\n")[0]}`);
  }
}

console.log("");
console.log(`${data.summary.passed}/${data.summary.total} passed`);

fs.rmSync(tmp, { recursive: true, force: true });

process.exit(data.summary.failed > 0 || data.summary.total === 0 ? 1 : 0);
