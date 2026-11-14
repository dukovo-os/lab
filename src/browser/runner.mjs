// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The runner of a `browser` step: dukovo-lab writes this file into the
// test's folder on the lab PC and starts it with node, once for each
// engine the step names. It opens the engine from the browser kit, gives
// the test's script, a file beside the test, an object `t` to drive the
// page the machine serves and to check it with axe-core, and tells
// dukovo-lab how it went by its exit status and the lines it prints:
// `screenshot:` and `trace:` for the files it keeps, `skip:` and `fail:`
// for why the engine did not pass. The exit status is 0 for a pass, 77
// (peer.rs's SKIP_STATUS) for a skip, 1 for a failure and 2 for a bad
// command line. docs/test-files.md, "Browser tests", describes the
// script's side, and src/browser.rs reads what this prints.

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

const FAILED = 1;
const BAD_COMMAND = 2;
const SKIPPED = 77;

const OPTIONS = ["kit", "engine", "url", "script", "shots", "logs", "name"];
const ENGINES = ["chromium", "firefox", "webkit"];

// A page of the console answers within a second or two, so a locator that
// is still not there after this long is a finding, not a slow machine.
const WAIT_MS = 15_000;

const AXE_TAGS = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"];

// A page test checks pages, not certificates: the console's TLS has tests
// of its own, so a certificate the lab made does not fail every page.
const CONTEXT_DEFAULTS = {
  ignoreHTTPSErrors: true,
  viewport: { width: 1280, height: 800 },
  locale: "en-US",
};

// What `t.skip` throws: this engine cannot do what the test needs here.
class Skip extends Error {}

function say(line) {
  console.log(line);
}

function finish(status, line) {
  say(line);
  process.exit(status);
}

function firstLine(error) {
  return messageOf(error).split("\n")[0];
}

// An error's message without the "Call log:" Playwright adds below it,
// which the failure's trace shows in full.
function messageOf(error) {
  const message = error instanceof Error ? error.message : String(error);
  return message.split("\nCall log:")[0].trimEnd();
}

function readArguments(words) {
  const given = {};
  for (let at = 0; at < words.length; at += 2) {
    const option = words[at];
    const name = option.slice(2);
    if (!option.startsWith("--") || !OPTIONS.includes(name)) {
      finish(BAD_COMMAND, `fail: ${option} is not an argument of the runner`);
    }
    if (name in given) finish(BAD_COMMAND, `fail: ${option} is given twice`);
    if (at + 1 === words.length) finish(BAD_COMMAND, `fail: ${option} has no value`);
    given[name] = words[at + 1];
  }
  const missing = OPTIONS.find((name) => !(name in given));
  if (missing) finish(BAD_COMMAND, `fail: --${missing} is missing`);
  if (!ENGINES.includes(given.engine)) {
    finish(BAD_COMMAND, "fail: --engine is chromium, firefox or webkit");
  }
  return given;
}

// Playwright and axe-core's script, from the kit. The engines are the
// kit's own unless the machine provides its own and says so, as the cloud
// build machines do with PLAYWRIGHT_BROWSERS_PATH.
function loadKit(kit) {
  const browsers = path.join(kit, "browsers");
  const provided = process.env.PLAYWRIGHT_BROWSERS_PATH;
  if (!provided && fs.statSync(browsers, { throwIfNoEntry: false })?.isDirectory()) {
    process.env.PLAYWRIGHT_BROWSERS_PATH = browsers;
  }
  const inKit = createRequire(path.join(kit, "package.json"));
  const need = (name, load) => {
    try {
      return load();
    } catch (error) {
      if (error.code !== "MODULE_NOT_FOUND") throw error;
      return finish(SKIPPED, `skip: the browser kit at ${kit} has no ${name}: ob lab makes it`);
    }
  };
  return {
    playwright: need("playwright", () => inKit("playwright")),
    axeScript: need("axe-core", () =>
      path.join(path.dirname(inKit.resolve("axe-core")), "axe.min.js"),
    ),
  };
}

const args = readArguments(process.argv.slice(2));
const { playwright, axeScript } = loadKit(args.kit);

const browserType = playwright[args.engine];
// Playwright throws for an engine it does not build for this system.
let executable;
try {
  executable = browserType.executablePath();
} catch (error) {
  finish(SKIPPED, `skip: ${args.engine} is not built for the lab PC's system: ${firstLine(error)}`);
}
if (!fs.statSync(executable, { throwIfNoEntry: false })?.isFile()) {
  finish(SKIPPED, `skip: ${args.engine} is not in the browser kit`);
}
let browser;
try {
  browser = await browserType.launch();
} catch (error) {
  // On Linux this is nearly always a system library the engine links
  // against, which `npx playwright install-deps` installs.
  finish(SKIPPED, `skip: ${args.engine} would not start on the lab PC: ${firstLine(error)}`);
}

// Each page the script opens with its context, so a failure can keep what
// they showed, and whether the script closed the context itself.
const opened = [];
let axeSource;

async function waitFor(locator, state, problem) {
  try {
    await locator.waitFor({ state, timeout: WAIT_MS });
  } catch (error) {
    if (!(error instanceof playwright.errors.TimeoutError)) throw error;
    throw new Error(`${locator} ${problem} after ${WAIT_MS / 1000} seconds`);
  }
}

const t = {
  url: args.url.replace(/\/+$/, ""),
  engine: args.engine,

  async open(pathname, options = {}) {
    const context = await browser.newContext({ ...CONTEXT_DEFAULTS, ...options });
    await context.tracing.start({ screenshots: true, snapshots: true });
    const page = await context.newPage();
    // Listed before it loads, so a page that never does is kept too.
    const entry = { context, page, closed: false };
    context.on("close", () => {
      entry.closed = true;
    });
    opened.push(entry);
    await page.goto(new URL(pathname, t.url).href, { waitUntil: "load" });
    return page;
  },

  see: (locator) => waitFor(locator, "visible", "is not visible"),

  gone: (locator) => waitFor(locator, "hidden", "is still there"),

  async violations(page) {
    axeSource ??= fs.readFileSync(axeScript, "utf8");
    // Run by the engine rather than added as a <script>, which a page's
    // content security policy stops: a page with a strict policy, as the
    // admin console's is, is checked as it is served.
    await page.evaluate(axeSource);
    return page.evaluate(async (tags) => {
      const { violations } = await window.axe.run(document, {
        runOnly: { type: "tag", values: tags },
      });
      return violations.map(({ id, help, nodes }) => ({
        id,
        help,
        targets: nodes.slice(0, 3).map((node) => node.target.flat().join(" ")),
      }));
    }, AXE_TAGS);
  },

  async axe(page) {
    const found = await t.violations(page);
    if (found.length > 0) {
      const lines = found.map(({ id, help, targets }) => `${id}: ${help} (${targets.join(", ")})`);
      throw new Error(lines.join("\n"));
    }
  },

  async screenshot(page, name) {
    if (!/^[A-Za-z0-9-]+$/.test(name)) {
      throw new Error(`screenshot name "${name}" may hold only letters, digits and -`);
    }
    const file = `${args.name}-${args.engine}-${name}.png`;
    await page.screenshot({ path: path.join(args.shots, file), fullPage: true });
    say(`screenshot: ${file}`);
  },

  skip(reason) {
    throw new Skip(reason);
  },
};

// Keeps a file a failure leaves. One that cannot be kept is told apart
// from the failure, which stays the first message.
async function keep(kind, file, write) {
  try {
    await write();
    say(`${kind}: ${file}`);
  } catch (error) {
    say(`fail: could not keep ${file}: ${firstLine(error)}`);
  }
}

async function keepWhatFailed() {
  for (const [index, { context, page, closed }] of opened.entries()) {
    // A context the script closed has nothing left to keep.
    if (closed) continue;
    const number = index + 1;
    if (!page.isClosed()) {
      const file = `${args.name}-${args.engine}-failed-${number}.png`;
      await keep("screenshot", file, () =>
        page.screenshot({ path: path.join(args.shots, file), fullPage: true }),
      );
    }
    const trace = `${args.name}-${args.engine}-trace-${number}.zip`;
    await keep("trace", trace, () => context.tracing.stop({ path: path.join(args.logs, trace) }));
  }
}

async function dropTraces() {
  for (const { context, closed } of opened) {
    // A pass stays a pass: the trace is thrown away whatever happens.
    if (!closed) await context.tracing.stop().catch(() => {});
  }
}

async function report(error) {
  if (error instanceof Skip) {
    say(`skip: ${error.message}`);
    return SKIPPED;
  }
  for (const line of messageOf(error).split("\n")) say(`fail: ${line}`);
  await keepWhatFailed();
  return FAILED;
}

let status = 0;
try {
  const check = (await import(pathToFileURL(args.script))).default;
  if (typeof check !== "function") throw new Error(`${args.script} has no default function to run`);
  await check(t);
  say("pass");
  await dropTraces();
} catch (error) {
  status = await report(error);
}
await browser.close();
process.exit(status);
