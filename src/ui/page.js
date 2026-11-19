// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The runner page's script. Everything it shows comes from a dukovo-lab
// command the server runs for it (see ui.rs), and everything it does is
// one of those commands: the last one run is shown at the bottom of the
// page, so a person can always see what to type to do the same.
//
// One view shows at a time, chosen by the address's hash (#home, #tests,
// #test/<id>, #failure/<run>/<machine>/<test>, #findings, #fleet, #runs,
// #run/<id>, #start, #phone), so a view can be shared and Back works. Text
// from tests, machines and logs is only ever set as text, never as HTML.
//
// Opened on the read-only address (`ui --share`), the server says so
// (`read_only` in /api/page) and the page leaves out every control marked
// `data-act`: the server refuses actions there anyway, so this is only so
// that nothing offered does nothing.

"use strict";

/** What the page knows; render() draws the current view from it. */
const state = {
  page: null, // { version, build, image, can_run, can_send, results }
  tests: [], // from `list --json`
  categories: [],
  presets: [],
  machines: [], // from `machines --json`
  schedules: null, // from `schedule --json`
  runs: [], // from `runs --json`, newest first
  grid: null, // from `grid --json`
  fleet: null, // from `fleet --json`
  findings: [], // from `findings --all --json`, most recently seen first
  current: null, // `status --json` of the run going now, if one is
  route: { name: "home", args: [] },
  // Per view.
  homeFilter: "failing",
  homeFind: "",
  testsFilter: "all",
  testsCategory: "",
  testsFind: "",
  details: new Map(), // test id to `show --json`
  detailsFor: "", // the runs the details were read at
  failure: null, // { key, results, ready }
  runView: null, // { id, status, results }
  picked: new Set(), // machines and pools to run on
  chosen: new Set(), // test ids to run
  preset: "",
  startFind: "",
  noting: new Map(), // finding id to the Not a bug note being typed
  classing: new Set(), // finding ids whose class is being changed
  sending: false,
  editing: null, // the schedule form's values while it is open
  stopping: false,
};

// ---- Talking to the server ----

/** Runs a reading command; answers its JSON. */
async function read(path) {
  const response = await fetch(path, { credentials: "same-origin" });
  if (response.status === 403) {
    notice("This page's key has changed: open the address dukovo-lab ui printed.");
    throw new Error("forbidden");
  }
  const reply = await response.json();
  showCommand(reply);
  return reply.answer !== undefined ? reply.answer : reply;
}

/** Runs an action (run, stop, done, send, findings); answers the command's JSON. */
async function act(path, body) {
  const response = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", "X-Dukovo-Lab": "1" },
    body: JSON.stringify(body),
  });
  const reply = await response.json();
  showCommand(reply);
  const answer = reply.answer || reply;
  if (answer.error) notice(answer.error);
  return answer;
}

function showCommand(reply) {
  if (reply && reply.command) {
    byId("last-command").textContent = "Last command: " + reply.command;
  }
}

// ---- Small helpers ----

function byId(id) {
  return document.getElementById(id);
}

/** A new element: el("span", { class: "chip" }, "text", child...). A `style` is a string of declarations. */
function el(tag, attributes, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes || {})) {
    if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
    // The page's policy refuses a style attribute; the style object is allowed.
    else if (name === "style") node.style.cssText = value;
    else if (value === true) node.setAttribute(name, "");
    else if (value !== false && value != null) node.setAttribute(name, value);
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(child instanceof Node ? child : String(child));
  }
  return node;
}

function notice(text) {
  const box = byId("notice");
  box.textContent = text;
  box.hidden = !text;
}

/** Whether the server says this address takes no actions. */
function readOnly() {
  return !!(state.page && state.page.read_only);
}

/** "45 s", "12 min", "3 h 40 min". */
function duration(seconds) {
  if (seconds < 60) return Math.max(0, Math.round(seconds)) + " s";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return minutes + " min";
  const hours = Math.floor(minutes / 60);
  return hours + " h" + (minutes % 60 ? " " + (minutes % 60) + " min" : "");
}

function plural(count, one, many) {
  return count + " " + (count === 1 ? one : many || one + "s");
}

/** A time from a bundle (UTC) as this PC shows it: "Today 07:10". */
function when(rfc3339) {
  if (!rfc3339) return "";
  const time = new Date(rfc3339);
  const clock = time.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const day = new Date(time);
  day.setHours(0, 0, 0, 0);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const days = Math.round((today - day) / 86400000);
  if (days === 0) return "Today " + clock;
  if (days === 1) return "Yesterday " + clock;
  return time.toLocaleDateString([], { month: "short", day: "numeric" }) + " " + clock;
}

/** `when`, for the middle of a sentence: "today 07:10". */
function whenLower(rfc3339) {
  return when(rfc3339).replace(/^(Today|Yesterday)/, (word) => word.toLowerCase());
}

/** "Nov 16" for a time from a bundle. */
function day(rfc3339) {
  return new Date(rfc3339).toLocaleDateString([], { month: "short", day: "numeric" });
}

/** The first line of a reason. */
function firstLine(text) {
  return (text || "").split("\n")[0];
}

function link(href, ...children) {
  return el("a", { href }, ...children);
}

/** The hash of a test's view. */
function testHash(id) {
  return "#test/" + id;
}

/** The hash of a failure's view. */
function failureHash(run, machine, test) {
  return "#failure/" + run + "/" + machine + "/" + test;
}

const CLASS_WORDS = { lab: "Lab", dukovo: "Dukovo", flaky: "Flaky", known: "Known", test: "Test" };

function classChip(cls) {
  if (!cls) return el("span", { class: "muted" }, "—");
  return el("span", { class: "chip " + (cls === "known" ? "plain" : cls) }, CLASS_WORDS[cls] || cls);
}

/** What the grid draws for a state: a glyph and the word beside it. */
const GLYPHS = {
  pass: ["✓", "passed"],
  fail: ["✕", "failed"],
  flaky: ["~", "flaky: failed, then passed when run again", "flaky"],
  stalled: ["◷", "stalled: no output", "stalled"],
  skip: ["–", "skipped"],
  running: ["▸", "running"],
  error: ["!", "lab fault: the harness or machine broke", "lab fault"],
  benched: ["!", "machine benched", "benched"],
};

function glyph(kind, tag, attributes) {
  const [mark, word] = GLYPHS[kind];
  const node = el(tag || "span", { class: "glyph st-" + kind, ...attributes }, mark, el("span", { class: "vh" }, " " + word));
  return node;
}

/** A result as "✕ failed", the glyph and its word. */
function resultWord(kind, text) {
  const [mark, long, short] = GLYPHS[kind];
  return el("span", { class: "word st-" + kind, title: long }, mark + " " + (text || short || long));
}

/** The colour-and-height strip of a history string, oldest first. */
function strip(history, label) {
  const words = { p: "passed", f: "failed", k: "flaky", e: "error", x: "skipped" };
  const bars = [...history].map((letter) => el("i", { class: letter, title: words[letter] }));
  return el("span", { class: "strip", role: "img", "aria-label": label + ": " + ([...history].map((l) => words[l]).join(", ") || "no results") }, bars);
}

/** Replaces a node's children while keeping the focus where it was. */
function drawInto(node, ...children) {
  const active = document.activeElement;
  const id = active && node.contains(active) ? active.id : "";
  const caret = id && active.selectionStart != null ? [active.selectionStart, active.selectionEnd] : null;
  node.replaceChildren(...children.flat());
  if (id) {
    const again = byId(id);
    if (again) {
      again.focus({ preventScroll: true });
      if (caret) again.setSelectionRange(...caret);
    }
  }
}

// ---- What machines and tests are ----

/** The column a machine's results go in: its pool, as --machine writes it, or itself. */
function columnOf(name) {
  const machine = state.machines.find((m) => m.name === name);
  return machine && machine.pool ? "pool:" + machine.pool : name;
}

/** The machines and pools a run can be asked for, in the grid's order. */
function columns() {
  if (state.grid) return state.grid.columns;
  const seen = [];
  for (const m of state.machines) {
    const name = m.pool ? "pool:" + m.pool : m.name;
    if (!seen.some((c) => c.name === name)) {
      seen.push({ name, kind: m.pool ? "pool" : "machine", members: [], spares: [] });
    }
  }
  return seen;
}

function isReal(column) {
  const machine = machineOfColumn(column);
  return !!machine && machine.driver !== "qemu";
}

/** A machine standing for a column: itself, or a pool's first member. */
function machineOfColumn(column) {
  if (!column.startsWith("pool:")) return state.machines.find((m) => m.name === column);
  const pool = column.slice(5);
  return state.machines.find((m) => m.pool === pool && !m.spare) || state.machines.find((m) => m.pool === pool);
}

/** How a test fits a machine or pool: "runs", "needs-person" or "lacks". */
function fit(test, column) {
  const machine = machineOfColumn(column);
  const entry = machine && test.machines.find((m) => m.machine === machine.name);
  return entry ? entry.fit : "runs";
}

function fitControls(test, column) {
  const machine = machineOfColumn(column);
  const entry = machine && test.machines.find((m) => m.machine === machine.name);
  return entry ? entry.controls : [];
}

/** Tests that need a person on a picked machine: they run only when ticked. */
function needsPerson(test) {
  return [...state.picked].some((column) => fit(test, column) === "needs-person");
}

/** About how long tests take on the picked machines, from earlier runs. */
function typicalSeconds(ids) {
  let longest = 0;
  for (const column of state.picked) {
    const machine = machineOfColumn(column);
    let total = 0;
    for (const id of ids) {
      const test = state.tests.find((t) => t.id === id);
      const last = test && machine && test.last_milliseconds[machine.name];
      if (last === undefined) return null;
      total += last;
    }
    longest = Math.max(longest, total);
  }
  return longest / 1000;
}

/** The finding with this id. */
function findingById(id) {
  return state.findings.find((f) => f.id === id);
}

// ---- Routes ----

function parseRoute() {
  const hash = location.hash.replace(/^#/, "");
  const [name, ...rest] = hash.split("/");
  const known = ["home", "tests", "test", "failure", "findings", "fleet", "runs", "run", "start", "phone"];
  if (!known.includes(name)) return { name: "home", args: [] };
  return { name, args: rest };
}

const NAV_OF = { home: "home", tests: "tests", test: "tests", failure: "findings", findings: "findings", fleet: "fleet", runs: "runs", run: "runs", start: "", phone: "" };

async function route() {
  state.route = parseRoute();
  for (const a of document.querySelectorAll("#nav a")) {
    if (a.dataset.view === NAV_OF[state.route.name]) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
  notice("");
  await loadRoute();
  render();
  // A new view starts at its top, with the screen reader told where it is.
  window.scrollTo(0, 0);
  const heading = byId("view").querySelector("h1");
  if (heading) {
    heading.tabIndex = -1;
    heading.focus({ preventScroll: true });
  }
}

/** Reads what the current view needs beyond what every view has. */
async function loadRoute() {
  const { name, args } = state.route;
  if (name === "test") await loadTest(args.join("/"));
  if (name === "tests" && state.grid === null) await loadGrid();
  if (name === "failure") await loadFailure();
  if (name === "run") await loadRun(args[0]);
  if (name === "start" && state.page === null) state.page = await read("/api/page");
  if (name === "phone" && !state.fleet) await loadFleet();
  if (name === "fleet" && !state.fleet) await loadFleet();
}

async function loadTest(id) {
  if (!id) return;
  if (state.details.has(id) && state.detailsFor === runsKey()) return;
  state.details.set(id, await read("/api/test?id=" + encodeURIComponent(id)));
  state.detailsFor = runsKey();
}

async function loadGrid() {
  state.grid = await read("/api/grid");
  state.gridKey = runsKey();
  state.gridAt = Date.now();
}

async function loadFleet() {
  state.fleet = await read("/api/fleet");
  state.fleetAt = Date.now();
}

async function loadRun(id) {
  if (!id) return;
  const status = await read("/api/status?run=" + id);
  const c = status.counts;
  const key = id + ":" + status.state + ":" + (c.passed + c.failed + c.skipped + c.errors);
  if (!state.runView || state.runView.id !== id || state.runView.key !== key) {
    const results = await read("/api/results?run=" + id);
    state.runView = { id, key, status, results };
  } else {
    state.runView.status = status;
  }
}

/** Reads the failure the address names: its run's results and its test's details. */
async function loadFailure() {
  const [run, machine, ...rest] = state.route.args;
  const test = rest.join("/");
  const key = [run, machine, test].join("/");
  if (!state.failure || state.failure.key !== key || state.failure.runsKey !== runsKey()) {
    state.failure = { key, runsKey: runsKey(), results: null };
    try {
      state.failure.results = await read("/api/results?run=" + run);
    } catch (err) {
      state.failure.results = null;
    }
    await loadTest(test);
    if (state.grid === null) await loadGrid();
  }
}

/** What changes when a run starts or ends or something is marked. */
function runsKey() {
  return state.runs.map((r) => r.id + r.state + r.sent).join();
}

// ---- Drawing ----

function render() {
  renderBar();
  const { name } = state.route;
  const views = { home: homeView, tests: testsView, test: testsView, failure: failureView, findings: findingsView, fleet: fleetView, runs: runsView, run: runView, start: startView, phone: phoneView };
  document.body.classList.toggle("phone-view", name === "phone");
  drawInto(byId("view"), views[name]());
  if (readOnly()) for (const control of document.querySelectorAll("[data-act]")) control.remove();
  // A console shows its end: that is where a test stopped.
  for (const pre of byId("view").querySelectorAll("pre.console")) pre.scrollTop = pre.scrollHeight;
}

/** The top bar's run: its progress and the time left, and the Start run button. */
function renderBar() {
  const page = state.page || {};
  const status = state.current;
  let text;
  let dotClass = "dot idle";
  let href = "#runs";
  if (status) {
    const c = status.counts;
    const total = c.passed + c.failed + c.skipped + c.errors + c.running + c.queued || 1;
    const done = Math.round((100 * (total - c.running - c.queued)) / total);
    text = "Run going, " + done + "%" + (status.left_seconds != null ? ", about " + duration(status.left_seconds) + " left" : "");
    dotClass = "dot";
    href = "#run/" + status.id;
  } else if (state.runs.length) {
    const last = state.runs[0];
    text = "No run going. Last: " + when(last.started) + (last.summary ? ", " + last.summary.passed + " passed, " + (last.summary.failed + last.summary.errors) + " failed" : "");
    href = "#run/" + last.id;
  } else {
    text = "No run yet";
  }
  const label = byId("runstate-text");
  label.setAttribute("href", href);
  label.replaceChildren(el("span", { class: dotClass }), el("span", {}, text));
  const start = byId("start-run");
  if (start) start.title = page.can_run === false ? "This page was opened without an image: open it with ob lab ui to start runs" : "";
  if (page.can_run === false && !page.read_only) {
    notice("This page was opened without an image, so it can show runs but not start them. Open it with ob lab ui.");
  }
}

/** A card with a title row and a body. */
function card(title, body, extra) {
  return el("section", { class: "card", "aria-label": title }, el("header", {}, el("h2", {}, title), extra), body);
}

// ---- Home: tests by machines ----

function failingCells(grid) {
  const cells = [];
  for (const row of grid.rows) {
    for (const [column, cell] of Object.entries(row.cells)) {
      if (cell.streak > 0) cells.push({ row, column, cell });
    }
  }
  return cells;
}

/** The numbers the summary tiles show, from the grid. */
function tallies(grid) {
  const failing = { total: 0, new: 0, known: 0, flaky: 0, test: 0, still: 0, lab: 0 };
  for (const row of grid.rows.filter((r) => r.failing)) {
    failing.total += 1;
    if (row.class === "lab") failing.lab += 1;
    else if (row.new) failing.new += 1;
    else if (row.class === "known") failing.known += 1;
    else if (row.class === "flaky") failing.flaky += 1;
    else if (row.class === "test") failing.test += 1;
    else failing.still += 1;
  }
  const lab = failingCells(grid).filter((f) => f.cell.class === "lab");
  const skips = new Map();
  let skipped = 0;
  for (const row of grid.rows) {
    for (const cell of Object.values(row.cells)) {
      if (cell.result !== "skip") continue;
      skipped += 1;
      skips.set(cell.skip_group, (skips.get(cell.skip_group) || 0) + 1);
    }
  }
  return { failing, lab, skipped, skips: [...skips.entries()].sort((a, b) => b[1] - a[1]) };
}

function homeView() {
  const grid = state.grid;
  if (!grid) return el("p", { class: "muted" }, "Reading the results…");
  const t = tallies(grid);
  return el("div", { class: "page-in", style: "display:flex;flex-direction:column;gap:20px" },
    el("h1", { class: "vh" }, "Home: tests by machines"),
    el("a", { class: "phone-link", href: "#phone" }, "Phone view: the round, new failures and machines on one screen →"),
    el("section", { class: "four", "aria-label": "At a glance" }, tiles(grid, t)),
    el("div", { class: "cols" },
      el("div", { class: "main" }, gridCard(grid)),
      el("div", { class: "side" }, needsLookCard(grid), runningCard())));
}

/** The four tiles at the top of Home. */
function tiles(grid, t) {
  const f = t.failing;
  const failingSub = [f.new + " new", f.known + " known", f.flaky + " flaky"];
  if (f.still) failingSub.push(f.still + " still failing");
  if (f.test) failingSub.push(f.test + " test wrong");
  if (f.lab) failingSub.push(f.lab + " lab");
  const benched = state.fleet ? state.fleet.machines.filter((m) => m.benched) : [];
  // A machine that is benched and has errors is one fault, not two.
  const faulty = new Set([...t.lab.map((f) => f.cell.machine), ...benched.map((m) => m.name)]);
  let labSub;
  if (benched.length) labSub = benched.map((m) => m.name + " benched: " + firstLine(m.benched.reason).replace(m.name + " was benched: ", "")).join("; ");
  else if (t.lab.length) labSub = firstLine(t.lab[0].cell.reason);
  else labSub = "no machine or harness faults in the latest results";
  const skipSub = t.skips.slice(0, 3).map(([why, n]) => n + " " + why);
  const round = grid.round;
  let roundBig = "—";
  let roundSub = "no run has ended yet";
  if (round) {
    roundBig = duration(round.seconds);
    roundSub = when(round.started) + (round.build ? ", build " + round.build : "");
    if (round.pools.length) {
      const split = round.pools.map((p) => "split over " + plural(p.machines, "machine") + " of " + p.name + "; " + duration(p.busy_seconds) + " of tests if run one after another");
      roundSub = split.join("; ");
    }
  }
  const tile = (label, big, sub, warn) =>
    el("div", { class: "card pad tile" }, el("div", { class: "label" }, label), el("div", { class: "big" + (warn ? " warn" : "") }, big), el("div", { class: "sub" }, sub));
  return [
    tile("Failing now", String(f.total), failingSub.join(" · ")),
    tile("Lab faults this round", String(faulty.size), labSub, faulty.size > 0),
    tile("Coverage lost to skips", String(t.skipped), skipSub.length ? skipSub.join(" · ") : "nothing skipped"),
    tile("Round time", roundBig, roundSub),
  ];
}

const HOME_FILTERS = [
  ["failing", "Failing", (r) => r.failing],
  ["new", "New", (r) => r.new],
  ["flaky", "Flaky", (r) => r.failing && (r.class === "flaky" || Object.values(r.cells).some((c) => c.state === "flaky"))],
  ["all", "All", () => true],
];

/** The tests-by-machines grid. */
function gridCard(grid) {
  const find = state.homeFind.toLowerCase();
  const filter = HOME_FILTERS.find(([key]) => key === state.homeFilter) || HOME_FILTERS[0];
  const buttons = HOME_FILTERS.map(([key, label, test]) =>
    el("button", { type: "button", "aria-pressed": String(key === state.homeFilter), onclick: () => {
      state.homeFilter = key;
      render();
    } }, label + " (" + grid.rows.filter(test).length + ")"));
  const rows = grid.rows
    .filter((r) => filter[2](r) && (!find || r.test.includes(find) || (r.class || "").includes(find)))
    .sort((a, b) => rowOrder(a) - rowOrder(b) || a.test.localeCompare(b.test));
  const running = new Map(grid.running.map((r) => [r.test + "|" + r.column, r]));
  const head = el("tr", {}, el("th", { scope: "col" }, "Test"), el("th", { scope: "col" }, "Class"),
    grid.columns.map((c) => el("th", { scope: "col", class: "c" }, c.name.replace(/^pool:/, ""),
      el("small", {}, c.kind === "pool" ? "pool, " + plural(c.members.length, "machine") + (c.spares.length ? " + " + plural(c.spares.length, "spare") : "") : columnKind(c.name)))),
    el("th", { scope: "col" }, "Last 14 runs"));
  const body = rows.map((row) => el("tr", { class: row.failing ? "hot" : "" },
    el("th", { scope: "row" }, el("a", { class: "plain", href: testHash(row.test) }, row.test), el("small", {}, sinceText(row))),
    el("td", {}, classChip(row.class)),
    grid.columns.map((c) => el("td", { class: "c" }, cellNode(row, c, running.get(row.test + "|" + c.name)))),
    el("td", {}, strip(row.history, "Last 14 runs of " + row.test))));
  const none = rows.length ? null : el("p", { class: "pad muted" }, state.homeFilter === "all" ? "No tests." : "Nothing here: no test is failing now" + (grid.runs ? "." : ", and no run has ended yet."));
  return el("section", { class: "card", "aria-label": "Tests by machines" },
    el("header", {}, el("h2", {}, "Tests by machines"), el("div", { class: "filters", role: "group", "aria-label": "Show" }, buttons),
      el("label", { class: "field grow", for: "home-find" }, "Find test",
        el("input", { type: "search", id: "home-find", value: state.homeFind, placeholder: "php, storage/…", oninput: (e) => {
          state.homeFind = e.target.value;
          render();
        } }))),
    el("div", { class: "scroll" }, el("table", { class: "tbl" }, el("thead", {}, head), el("tbody", {}, body))),
    none,
    el("div", { class: "legend" }, [["pass", "passed"], ["fail", "failed"], ["flaky", "flaky, passed on rerun"], ["stalled", "stalled, no output"], ["error", "lab fault"], ["skip", "skipped"], ["running", "running"]].map(([k, w]) =>
      el("span", {}, el("b", { class: "word st-" + k }, GLYPHS[k][0]), " " + w)), el("span", {}, "blank: not run there")));
}

/** Red rows first, new ones before the rest. */
function rowOrder(row) {
  if (row.new) return 0;
  if (row.failing && row.class === "dukovo") return 1;
  if (row.failing) return 2;
  return 3;
}

function columnKind(name) {
  const machine = state.machines.find((m) => m.name === name);
  return machine ? machine.driver : "";
}

/** "new tonight", "red since Nov 16": when a row went red. */
function sinceText(row) {
  if (!row.failing || !row.red_since) return row.class ? "" : row.category;
  const first = row.red_since;
  if (row.new) return "new " + whenLower(first.at);
  return "red since " + day(first.at);
}

/** One cell of the grid: its glyph, a link to what it was. */
function cellNode(row, column, running) {
  const cell = row.cells[column.name];
  if (running) return glyph("running", "a", { href: "#test/" + row.test, title: running.machine + " is running it now" });
  if (!cell) return el("span", { class: "vh" }, "not run here");
  const title = [cell.machine, GLYPHS[cell.state][1], cell.reason ? "— " + cell.reason : ""].filter(Boolean).join(" ");
  if (cell.streak > 0) return glyph(cell.state, "a", { href: failureHash(cell.run, cell.machine, row.test), title });
  return glyph(cell.state, "a", { href: testHash(row.test), title });
}

/** "Needs a look": new Dukovo failures and lab faults, newest first. */
function needsLookCard(grid) {
  const items = failingCells(grid)
    .filter(({ cell }) => (cell.class === "dukovo" && cell.streak === 1) || cell.class === "lab")
    .sort((a, b) => b.cell.at.localeCompare(a.cell.at))
    .slice(0, 8);
  const lis = items.map(({ row, cell }) => {
    const lab = cell.class === "lab";
    return el("li", { style: "display:flex;flex-direction:column;gap:4px" },
      el("a", { href: failureHash(cell.run, cell.machine, row.test), style: "font-weight:600" }, row.test + (lab ? ": lab fault" : " fails") + " on " + cell.machine),
      el("span", { class: "muted" }, (lab ? "Lab: " : "New " + whenLower(cell.at) + ". Dukovo: ") + (cell.reason || "")));
  });
  return card("Needs a look", el("div", { class: "pad" }, lis.length ? el("ol", { class: "plain" }, lis) : el("p", { class: "muted", style: "margin:0" }, "No new Dukovo failures and no lab faults in the latest results.")));
}

/** "Running now": each busy machine's test and progress, and the lab PC's load. */
function runningCard() {
  const fleet = state.fleet;
  const busy = fleet ? fleet.machines.filter((m) => m.doing) : [];
  const rows = busy.map((m) => {
    const d = m.doing;
    let percent = null;
    let age = "";
    if (d.started) {
      const seconds = (Date.now() - new Date(d.started)) / 1000;
      age = duration(seconds);
      if (d.typical_milliseconds) percent = Math.min(95, (100 * seconds * 1000) / d.typical_milliseconds);
    }
    const progress = el("div", { class: "track" });
    if (percent != null) progress.append(el("i", { style: "width:" + percent.toFixed(0) + "%" }));
    const test = d.test || "starting";
    const tally = d.total != null ? " · " + d.done + " of " + d.total + " tests" : d.done ? " · " + d.done + " tests ended" : "";
    return el("div", {}, el("div", { style: "display:flex;justify-content:space-between;gap:8px" },
      el("a", { href: "#run/" + d.run, class: "plain", style: "color:inherit;text-decoration:none" }, m.name + " · " + test), el("span", { class: "muted" }, age)),
      percent != null ? progress : null, el("div", { class: "muted small" }, tally.replace(/^ · /, "")));
  });
  const load = fleet && fleet.lab_pc_load != null ? "Lab PC load " + Math.round(fleet.lab_pc_load * 100) + "% of its CPUs" + (fleet.lab_pc_load > 0.75 ? ", busy: browser and performance times may be slow" : ", normal") : "Lab PC load not known";
  return card("Running now", el("div", { class: "pad", style: "display:flex;flex-direction:column;gap:10px;font-size:13px" },
    rows.length ? rows : el("div", { class: "muted" }, "No machine is running a test."), el("div", { class: "muted" }, load)));
}

// ---- Phone: the round at a glance, for a screen 390 px wide ----

/** A machine's state on the phone: a glyph, a word, and the reason a benched machine is out. */
function phoneMachine(machine) {
  let kind = "idle";
  let word = "idle";
  let detail = "";
  if (machine.benched) {
    kind = "benched";
    word = "benched";
    detail = firstLine(machine.benched.reason).replace(machine.name + " was benched: ", "");
  } else if (machine.doing) {
    kind = "running";
    word = machine.spare ? "spare, rerunning" : "running";
    detail = machine.doing.test || "";
  } else if (machine.spare) {
    word = "spare";
  }
  const mark = kind === "benched" ? "! " : kind === "running" ? "▸ " : "";
  return el("li", { class: "pm" },
    el("span", { class: "pm-name" }, machine.name),
    el("span", { class: "pm-state is-" + kind }, mark + word),
    detail ? el("span", { class: "pm-detail" }, detail) : null);
}

/** The failures that are new since the last round: a Dukovo failure that is red for the first time, or a lab fault, a row each. */
function phoneNew(grid) {
  const rows = [];
  for (const row of grid.rows) {
    const cells = Object.entries(row.cells).map(([column, cell]) => ({ column, cell }))
      .filter(({ cell }) => cell.streak > 0 && ((cell.class === "dukovo" && cell.streak === 1) || cell.class === "lab"));
    if (!cells.length) continue;
    cells.sort((a, b) => b.cell.at.localeCompare(a.cell.at));
    rows.push({ row, cells, at: cells[0].cell.at });
  }
  rows.sort((a, b) => b.at.localeCompare(a.at));
  return rows;
}

function phoneView() {
  const grid = state.grid;
  const page = state.page || {};
  const status = state.current;
  const last = state.runs.find((r) => r.state !== "running" && r.summary);
  let title;
  let counts;
  let percent = null;
  let timeLeft = "";
  if (status) {
    const c = status.counts;
    const total = c.passed + c.failed + c.skipped + c.errors + c.running + c.queued || 1;
    percent = Math.round((100 * (total - c.running - c.queued)) / total);
    const run = state.runs.find((r) => r.id === status.id);
    const named = run && (run.schedule || run.preset);
    title = (named ? runName(run).replace(" (scheduled)", "") : "Round") + ", " + percent + "%";
    counts = { passed: c.passed, failing: c.failed + c.errors, left: c.running + c.queued };
    if (status.left_seconds != null) {
      const ends = new Date(Date.now() + status.left_seconds * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit", timeZoneName: "short" });
      timeLeft = "About " + duration(status.left_seconds) + " left · ends near " + ends;
    } else {
      timeLeft = "Time left not known: some of its tests have not run here before";
    }
  } else if (last) {
    const s = last.summary;
    title = "No round going";
    counts = { passed: s.passed, failing: s.failed + s.errors, left: 0 };
    timeLeft = "The last one ended " + whenLower(last.ended || last.started);
  } else {
    title = "No round yet";
    counts = { passed: 0, failing: 0, left: 0 };
  }
  const tile = (label, n, cls) => el("div", { class: "card ptile" }, el("div", { class: "ptile-label" }, label), el("div", { class: "ptile-n " + (cls || "") }, String(n)));
  const fresh = grid ? phoneNew(grid) : [];
  const items = fresh.slice(0, 6).map(({ row, cells }) => {
    const lab = cells[0].cell.class === "lab";
    const machines = [...new Set(cells.map(({ cell }) => cell.machine))].join(", ");
    return el("li", {},
      el("a", { href: failureHash(cells[0].cell.run, cells[0].cell.machine, row.test), class: "pn-test" }, row.test),
      el("div", { class: "pn-why" }, (lab ? "Lab fault" : "Dukovo") + " · " + machines));
  });
  const more = fresh.length > items.length ? el("a", { href: "#home", class: "pn-more" }, "and " + (fresh.length - items.length) + " more, on Home") : null;
  const newList = !grid ? el("p", { class: "muted small", style: "margin:0" }, "Reading the results…")
    : items.length ? el("ul", { class: "pn" }, items) : el("p", { class: "muted small", style: "margin:0" }, "Nothing new since the last round.");
  const machines = state.fleet ? state.fleet.machines.map(phoneMachine) : [];
  const pushLine = page.notify
    ? "You will get one notice when the round ends, with the counts and the new failures."
    : "No push notice is set up. notify.toml in the lab's configuration folder turns one on.";
  return el("div", { class: "phone" },
    el("header", { class: "phone-head" },
      el("div", { class: "phone-kicker" }, "Dukovo lab · " + (page.read_only ? "read only" : "phone view")),
      el("h1", { class: "phone-title" }, title),
      percent != null ? el("div", { class: "track phone-track", role: "img", "aria-label": percent + " percent done" }, el("i", { style: "width:" + percent + "%" })) : null,
      timeLeft ? el("div", { class: "phone-left" }, timeLeft) : null),
    el("div", { class: "phone-body" },
      el("section", { class: "ptiles", "aria-label": "Counts" }, tile("Passed", counts.passed, "pass"), tile("Failing", counts.failing, counts.failing ? "fail" : ""), tile("Left", counts.left)),
      el("section", { class: "card pad", "aria-label": "New since last round" }, el("h2", { class: "phone-h2" }, "New since last round"), newList, more),
      el("section", { class: "card pad", "aria-label": "Machines" }, el("h2", { class: "phone-h2" }, "Machines"),
        machines.length ? el("ul", { class: "pms" }, machines) : el("p", { class: "muted small", style: "margin:0" }, "Reading the machines…")),
      el("div", { class: "callout phone-push" }, pushLine),
      el("a", { class: "phone-all", href: "#home" }, "All tests, machines and runs →")));
}

// ---- Tests: details and who can run them ----

const TEST_FILTERS = [
  ["all", "All"],
  ["alone", "Run alone"],
  ["person", "Needs a person"],
  ["nowhere", "Can't run anywhere real"],
];

function fitsOf(test) {
  return columns().map((c) => fit(test, c.name));
}

/** Whether a test passes a Tests filter. */
function passesTestFilter(key, test) {
  const fits = fitsOf(test);
  if (key === "alone") return !fits.includes("needs-person") && fits.includes("runs");
  if (key === "person") return fits.includes("needs-person");
  if (key === "nowhere") {
    const real = columns().filter((c) => isReal(c.name));
    return real.length > 0 && real.every((c) => fit(test, c.name) === "lacks");
  }
  return true;
}

/** How many of each count, by a name, most first: "20 usb, 4 link". */
function countNames(names) {
  const counts = new Map();
  for (const name of names) counts.set(name, (counts.get(name) || 0) + 1);
  return [...counts.entries()].sort((a, b) => b[1] - a[1]).map(([name, n]) => n + " " + name);
}

/** The per-machine bars at the top of Tests: how many tests run alone, need a person, can't run. */
function fitBars() {
  const bars = columns().map((column) => {
    const runs = [];
    const person = [];
    const lacks = [];
    for (const test of state.tests) {
      const kind = fit(test, column.name);
      (kind === "runs" ? runs : kind === "needs-person" ? person : lacks).push(test);
    }
    const total = state.tests.length || 1;
    const part = (cls, list) => {
      const bit = el("i", { class: cls });
      bit.style.width = (100 * list.length) / total + "%";
      return bit;
    };
    const sentence = [runs.length + " run alone"];
    if (person.length) sentence.push(" · ", el("b", {}, person.length + " need a person"), " (" + countNames(person.flatMap((t) => fitControls(t, column.name))).join(", ") + ")");
    if (lacks.length) sentence.push(" · " + lacks.length + " can't run (" + countNames(lacks.flatMap((t) => fitControls(t, column.name))).slice(0, 3).map((n) => "no " + n.replace(/^\d+ /, "")).join(", ") + ")");
    return el("div", { class: "card pad" },
      el("div", { style: "font-size:15px;font-weight:600" }, column.name.replace(/^pool:/, ""), column.kind === "pool" ? el("span", { class: "muted", style: "font-weight:400" }, " pool of " + plural(column.members.length, "machine")) : null),
      el("div", { class: "sumbar", role: "img", "aria-label": column.name + ": " + runs.length + " run alone, " + person.length + " need a person, " + lacks.length + " can't run" }, part("runs", runs), part("person", person), part("lacks", lacks)),
      el("div", { class: "small" }, sentence));
  });
  return el("section", { class: "three", "aria-label": "Who can run the tests" }, bars);
}

function fitChip(test, column) {
  const kind = fit(test, column);
  const controls = fitControls(test, column).join(", ");
  if (kind === "runs") return el("span", { class: "chip pass" }, "runs");
  if (kind === "needs-person") return el("span", { class: "chip flaky" }, "person: " + controls);
  return el("span", { class: "chip plain" }, "can't: " + controls);
}

function testsView() {
  if (!state.tests.length) return el("p", { class: "muted" }, "Reading the tests…");
  const selected = state.route.name === "test" ? state.route.args.join("/") : "";
  const find = state.testsFind.toLowerCase();
  const shown = state.tests.filter((t) =>
    passesTestFilter(state.testsFilter, t) &&
    (!state.testsCategory || t.category === state.testsCategory) &&
    (!find || t.id.includes(find) || t.about.toLowerCase().includes(find) || t.controls.some((c) => c.includes(find)) || t.steps.some((s) => s.step.toLowerCase().includes(find))));
  const cols = columns();
  const buttons = TEST_FILTERS.map(([key, label]) => {
    const count = state.tests.filter((t) => passesTestFilter(key, t)).length;
    return el("button", { type: "button", "aria-pressed": String(key === state.testsFilter), onclick: () => {
      state.testsFilter = key;
      render();
    } }, label + (key === "all" ? "" : " (" + count + ")"));
  });
  const select = el("select", { id: "tests-category", onchange: (e) => {
    state.testsCategory = e.target.value;
    render();
  } }, el("option", { value: "" }, "All " + state.categories.length), state.categories.map((c) => el("option", { value: c.id, selected: c.id === state.testsCategory }, c.title)));
  const rows = shown.map((test) => el("tr", { class: test.id === selected ? "sel" : "" },
    el("th", { scope: "row" }, el("a", { class: "plain", href: testHash(test.id), "aria-current": test.id === selected ? "true" : null }, test.id)),
    el("td", { class: "muted" }, test.time),
    cols.map((c) => el("td", {}, fitChip(test, c.name)))));
  const table = card(plural(shown.length, "test") + (shown.length === state.tests.length ? "" : " of " + state.tests.length), [
    el("div", { class: "scroll" }, el("table", { class: "tbl" },
      el("thead", {}, el("tr", {}, el("th", { scope: "col" }, "Test"), el("th", { scope: "col" }, "Limit"), cols.map((c) => el("th", { scope: "col" }, c.name.replace(/^pool:/, ""))))),
      el("tbody", {}, rows))),
    shown.length ? null : el("p", { class: "pad muted" }, "No test matches."),
    el("div", { class: "legend" },
      el("span", {}, el("span", { class: "chip pass" }, "runs"), " the lab does it all"),
      el("span", {}, el("span", { class: "chip flaky" }, "person: usb"), " a person does a step, the rest is automatic"),
      el("span", {}, el("span", { class: "chip plain" }, "can't: disc"), " the machine has no way to do a step")),
  ], [el("div", { class: "filters", role: "group", "aria-label": "Show" }, buttons),
    el("label", { class: "field", for: "tests-category" }, "Category", select),
    el("label", { class: "field grow", for: "tests-find" }, "Find",
      el("input", { type: "search", id: "tests-find", value: state.testsFind, placeholder: "name, step or control", oninput: (e) => {
        state.testsFind = e.target.value;
        render();
      } }))]);
  return el("div", { style: "display:flex;flex-direction:column;gap:20px" },
    el("h1", { class: "vh" }, "Tests"), fitBars(),
    el("div", { class: "cols" }, el("div", { class: "main" }, table), el("aside", { class: "side", style: "flex-basis:420px", "aria-label": "Test details" }, testPanel(selected))));
}

/** The panel at the right of Tests: one test in full. */
function testPanel(id) {
  if (!id) return el("div", { class: "card pad muted" }, "Pick a test to see what it does, which steps need a person and how it fits each machine.");
  const test = state.tests.find((t) => t.id === id);
  const data = state.details.get(id);
  if (!test) return el("div", { class: "card pad" }, "There is no test " + id + ".");
  if (!data) return el("div", { class: "card pad muted" }, "Reading " + id + "…");
  const category = state.categories.find((c) => c.id === test.category);
  const cols = columns();
  const lastMachine = ["qemu", ...Object.keys(data.last_milliseconds)].find((m) => data.last_milliseconds[m] !== undefined);
  const last = lastMachine ? " · last took " + duration(data.last_milliseconds[lastMachine] / 1000) + " on " + lastMachine : " · has not run yet";
  const needs = cols.filter((c) => fit(test, c.name) === "needs-person");
  const lackingNames = cols.filter((c) => fit(test, c.name) === "lacks").map((c) => c.name.replace(/^pool:/, ""));
  const runsNames = cols.filter((c) => fit(test, c.name) === "runs").map((c) => c.name.replace(/^pool:/, ""));
  const personBoxes = needs.map((column) => {
    const controls = fitControls(test, column.name);
    const steps = test.steps.map((s, i) => ({ n: i + 1, ...s })).filter((s) => controls.includes(s.control));
    const covered = new Set(steps.map((s) => s.control));
    const items = steps.map((s) => el("li", {}, "Step " + s.n + ": " + s.step));
    for (const control of controls.filter((c) => !covered.has(c))) items.push(el("li", {}, control + ": as the machine boots"));
    const rest = [runsNames.length ? "Runs alone on " + runsNames.join(", ") + "." : "", lackingNames.length ? " " + lackingNames.join(", ") + " can't run it." : ""].join("");
    return el("div", { class: "callout" }, el("div", { style: "font-weight:600" }, "Needs a person on " + column.name.replace(/^pool:/, "")), el("ol", {}, items),
      el("div", { class: "fine" }, "The run waits with a Done button at each of these. " + rest));
  });
  const stepRows = test.steps.map((s, i) => {
    const person = needs.some((c) => fitControls(test, c.name).includes(s.control));
    return el("li", {}, el("span", { class: "n" }, i + 1), el("span", { class: "chip ctl " + (person ? "flaky" : "plain") }, s.control || "lab"), el("span", { class: "txt" }, s.step));
  });
  const target = state.detailMachine && cols.some((c) => c.name === state.detailMachine) ? state.detailMachine : (cols.find((c) => fit(test, c.name) === "runs") || cols[0] || { name: "qemu" }).name;
  const choose = el("select", { id: "detail-machine", "aria-label": "Machine to run on", "data-act": true, onchange: (e) => {
    state.detailMachine = e.target.value;
    render();
  } }, cols.map((c) => el("option", { value: c.name, selected: c.name === target }, c.name.replace(/^pool:/, ""))));
  const kind = fit(test, target);
  const run = el("button", { class: "btn ink", type: "button", disabled: !state.page || !state.page.can_run || kind === "lacks",
    title: kind === "lacks" ? "That machine cannot do " + fitControls(test, target).join(", ") : null,
    onclick: () => startRun([id], [target]), "data-act": true }, "Run on " + target.replace(/^pool:/, ""));
  const shown = state.showHistory === id;
  return el("div", { class: "card pad" },
    el("div", { class: "small muted" }, category ? category.title : test.category),
    el("h2", { style: "margin:4px 0 8px;font-size:18px;font-weight:700" }, id),
    el("p", { style: "margin:0 0 14px;line-height:1.5" }, test.about),
    el("dl", { class: "facts", style: "margin-bottom:16px" },
      el("dt", {}, "Time limit"), el("dd", {}, test.time + last),
      el("dt", {}, "Uses"), el("dd", {}, test.controls.join(", ") + (data.host_needs.length ? ", lab PC: " + data.host_needs.join(", ") : "")),
      el("dt", {}, "File"), el("dd", { class: "mono" }, test.file)),
    personBoxes.length ? el("div", { style: "display:flex;flex-direction:column;gap:12px;margin-bottom:16px" }, personBoxes) : null,
    el("h3", { style: "margin-bottom:8px" }, plural(test.steps.length, "step")),
    el("ol", { class: "stepl" }, stepRows),
    el("div", { class: "row2", style: "margin-top:16px" }, choose, run,
      el("button", { class: "btn", type: "button", "aria-expanded": String(shown), onclick: () => {
        state.showHistory = shown ? "" : id;
        render();
      } }, "History")),
    shown ? historyTable(data) : null);
}

/** A test's newest results on every machine. */
function historyTable(data) {
  if (!data.history.length) return el("p", { class: "muted small" }, "No run has ended with this test yet.");
  const rows = data.history.map((past) => {
    const kind = past.result === "pass" ? "pass" : past.result === "skip" ? "skip" : past.result === "error" ? "error" : past.flaky ? "flaky" : "fail";
    const red = kind === "fail" || kind === "flaky" || kind === "error";
    return el("tr", {}, el("td", {}, when(past.at)), el("td", {}, past.machine),
      el("td", {}, red ? el("a", { href: failureHash(past.run, past.machine, data.id) }, resultWord(kind)) : resultWord(kind)),
      el("td", { class: "small muted" }, past.reason || ""));
  });
  return el("div", { class: "scroll", style: "margin-top:12px" }, el("table", { class: "tbl" },
    el("thead", {}, el("tr", {}, ["When", "Machine", "Result", "Why"].map((h) => el("th", { scope: "col" }, h)))), el("tbody", {}, rows)));
}

// ---- Failure: one failure, triaged ----

const STATE_WORDS = { new: "New", "not-a-bug": "Not a bug", "fix-proposed": "Fix proposed", approved: "Fix approved", fixed: "Fixed" };

/** Whether a finding failed in a run lab-results does not have yet. */
function unsent(finding) {
  return finding.runs.some((seen) => !seen.sent);
}

/** Whether Send has something of a finding's to send. */
function sendable(finding) {
  return finding.state !== "not-a-bug" && unsent(finding);
}

/** A log file's text, fetched once: undefined until it is here, null when there is none. */
function logText(url) {
  if (!url) return null;
  if (!state.logs) state.logs = new Map();
  if (state.logs.has(url)) return state.logs.get(url);
  state.logs.set(url, undefined);
  fetch(url, { credentials: "same-origin" })
    .then((r) => (r.ok ? r.text() : null))
    .catch(() => null)
    .then((text) => {
      state.logs.set(url, text);
      if (["failure", "run"].includes(state.route.name)) render();
    });
  return undefined;
}

function median(numbers) {
  if (!numbers.length) return null;
  const sorted = [...numbers].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}

/** What the class says about why, in a sentence. */
function whyText(cls, result, finding) {
  let text;
  if (cls === "lab") text = "an error: the harness or the machine broke, so it says nothing about Dukovo and is never sent";
  else if (cls === "flaky") text = "failed, then passed when run again" + (result.rerun ? " on " + result.rerun.machine : "");
  else if (cls === "known") text = finding && finding.fix ? "its finding links to " + finding.fix : "its finding is " + (finding ? STATE_WORDS[finding.state].toLowerCase() : "known");
  else if (cls === "test") text = "a person said the test is wrong" + (finding && finding.note ? ": " + finding.note : "");
  else text = result.rerun ? "failed, and failed again when run again on " + result.rerun.machine + ": not flaky" : "a failure, not flaky";
  return (finding && finding.class ? "set by a person: " : "set by the lab: ") + text;
}

function failureView() {
  const [runId, machine, ...rest] = state.route.args;
  const test = rest.join("/");
  const crumb = el("a", { class: "crumb", href: "#home" }, "← Tests by machines");
  const f = state.failure;
  if (!f || f.results === null) {
    return el("div", {}, crumb, el("h1", {}, test), el("p", { class: "muted" }, f && f.results === null && f.key ? "That run is not in the store." : "Reading the run…"));
  }
  const result = f.results.results.find((r) => r.machine === machine && r.test === test);
  if (!result) return el("div", {}, crumb, el("h1", {}, test), el("p", {}, "Run " + runId + " has no result of " + test + " on " + machine + "."));
  const entry = f.results.classes.find((c) => c.machine === machine && c.test === test);
  const finding = entry && entry.finding ? findingById(entry.finding) : null;
  const cls = entry ? entry.class : null;
  const data = state.details.get(test);
  const row = state.grid && state.grid.rows.find((r) => r.test === test);
  const cell = row && row.cells[columnOf(machine)];
  const kind = result.result === "pass" ? "pass" : result.result === "skip" ? "skip" : result.result === "error" ? "error" : result.rerun && result.rerun.result === "pass" ? "flaky" : "fail";
  const title = test + (kind === "error" ? ": lab fault" : kind === "pass" ? " passed" : kind === "skip" ? " skipped" : kind === "flaky" ? " is flaky" : " fails") + " on " + machine;
  const facts = [];
  if (cell && cell.red_since && cell.run === runId) facts.push(cell.streak === 1 ? "new in " + when(cell.red_since.at) : "red since " + day(cell.red_since.at) + (cell.red_since.build ? ", build " + cell.red_since.build : ""));
  if (finding) facts.push(finding.count === 1 ? "failed once" : "failed " + finding.count + " times on " + finding.machines.join(", "));
  if (finding && Object.keys(finding.passing || {}).length) facts.push("passes again on " + Object.keys(finding.passing).join(", "));
  return el("div", { style: "display:flex;flex-direction:column;gap:20px" },
    el("div", { class: "row2", style: "align-items:flex-start" },
      el("div", { style: "flex:1 1 520px;min-width:0" }, crumb,
        el("h1", { style: "margin:8px 0 6px;font-size:26px;font-weight:700;overflow-wrap:anywhere" }, title),
        el("div", { class: "row2 small muted" }, classChip(cls), el("span", {}, whyText(cls, result, finding)), facts.flatMap((t) => [el("span", {}, "·"), el("span", {}, t)]))),
      failureActions(result, finding, cls)),
    el("div", { class: "three" }, otherMachinesCard(test, machine, result, row), nightsCard(cell, finding), conditionsCard(runId, machine, test, result, data)),
    stepsCard(f.results, runId, machine, test, result),
    notesCard(finding));
}

/** The buttons above a failure: Rerun, Change class, Send, Not a bug. */
function failureActions(result, finding, cls) {
  if (readOnly()) return null;
  const column = columnOf(result.machine);
  const can = state.page || {};
  const rerun = el("button", { class: "btn", type: "button", disabled: !can.can_run, onclick: () => startRun([result.test], [column]) }, "Rerun on " + column.replace(/^pool:/, "pool "));
  const buttons = [rerun];
  if (finding && state.classing.has(finding.id)) {
    const select = el("select", { id: "class-select", "aria-label": "Class" },
      el("option", { value: "auto" }, "Worked out by the lab"),
      Object.entries(CLASS_WORDS).map(([key, word]) => el("option", { value: key, selected: finding.class === key }, word)));
    const apply = async () => {
      const answer = await act("/api/findings", { finding: finding.id, class: select.value });
      if (!answer.error) state.classing.delete(finding.id);
      await reloadFindings();
    };
    return el("div", { class: "nab" }, el("label", { class: "field", for: "class-select" }, "Class", select),
      el("button", { class: "btn ink", type: "button", onclick: apply }, "Set class"),
      el("button", { class: "btn", type: "button", onclick: () => {
        state.classing.delete(finding.id);
        render();
      } }, "Cancel"));
  }
  if (finding && state.noting.has(finding.id)) return notingForm(finding);
  buttons.push(el("button", { class: "btn", type: "button", disabled: !finding,
    title: finding ? null : "Only a failure has a finding to give a class to; a lab fault is always the lab's",
    onclick: () => {
      state.classing.add(finding.id);
      render();
    } }, "Change class"));
  if (finding && finding.state === "not-a-bug") {
    buttons.push(el("button", { class: "btn", type: "button", onclick: () => changeFinding({ finding: finding.id, reopen: true }) }, "Reopen"));
  } else if (finding) {
    buttons.push(el("button", { class: "btn", type: "button", onclick: () => {
      state.noting.set(finding.id, "");
      render();
    } }, "Not a bug"));
  }
  if (finding && finding.state !== "not-a-bug") {
    const canSend = !!can.can_send;
    buttons.push(el("button", { class: "btn ink", type: "button", disabled: !canSend || state.sending || !unsent(finding),
      title: canSend ? null : "Send needs a lab-results clone: ob lab makes it",
      onclick: () => sendNow({ findings: [finding.id] }) }, unsent(finding) ? (state.sending ? "Sending…" : "Send to lab-results") : "Sent to lab-results"));
  }
  return el("div", { class: "row2" }, buttons);
}

/** Not a bug, with the reason typed in place. */
function notingForm(finding) {
  const say = () => changeFinding({ finding: finding.id, not_a_bug: true, note: byId("nab-note").value });
  setTimeout(() => byId("nab-note") && document.activeElement !== byId("nab-note") && byId("nab-note").focus(), 0);
  return el("div", { class: "nab" },
    el("label", { class: "vh", for: "nab-note" }, "Why it is not a bug"),
    el("input", { type: "text", id: "nab-note", maxlength: "500", placeholder: "Why, if you like: the test is wrong, the cable was out…", value: state.noting.get(finding.id),
      oninput: (e) => state.noting.set(finding.id, e.target.value), onkeydown: (e) => e.key === "Enter" && say() }),
    el("button", { class: "btn ink", type: "button", onclick: say }, "Say not a bug"),
    el("button", { class: "btn", type: "button", onclick: () => {
      state.noting.delete(finding.id);
      render();
    } }, "Cancel"));
}

/** Same test, other machines: the newest result in each column, and the rerun. */
function otherMachinesCard(test, machine, result, row) {
  const rows = [];
  if (row) {
    for (const column of state.grid.columns) {
      const cell = row.cells[column.name];
      if (!cell) continue;
      const here = cell.machine === machine;
      rows.push(el("tr", {}, el("th", { scope: "row" }, column.name.replace(/^pool:/, "pool ") + (column.kind === "pool" ? " (" + cell.machine + ")" : "")),
        el("td", {}, resultWord(cell.state)), el("td", { class: "muted", style: "text-align:right" }, when(cell.at) + (here ? ", this run" : ""))));
    }
  }
  if (result.rerun) {
    const kind = result.rerun.result === "pass" ? "pass" : result.rerun.result === "error" ? "error" : "fail";
    rows.push(el("tr", {}, el("th", { scope: "row" }, result.rerun.machine), el("td", {}, resultWord(kind)), el("td", { class: "muted", style: "text-align:right" }, "rerun, this run")));
  }
  const red = row ? Object.values(row.cells).filter((c) => c.streak > 0).length : 0;
  const ran = row ? Object.keys(row.cells).length : 0;
  return card("Same test, other machines", el("div", { class: "pad" },
    rows.length ? el("table", { class: "tbl", style: "font-size:13px" }, el("tbody", {}, rows)) : el("p", { class: "muted small" }, "No other result of this test."),
    ran > 1 ? el("p", { class: "muted small", style: "margin:12px 0 0" }, "Red in " + red + " of the " + ran + " places it ran.") : null));
}

/** The last 14 results where this test ran, and when it first went red. */
function nightsCard(cell, finding) {
  const history = cell ? cell.history : "";
  const padded = ".".repeat(Math.max(0, 14 - history.length)) + history;
  const words = { p: "passed", f: "failed", k: "flaky", e: "error", x: "skipped", ".": "no result" };
  const bars = [...padded].map((letter) => el("i", { class: letter === "." ? "none" : letter, title: words[letter] }));
  return card("Last 14 runs", el("div", { class: "pad" },
    el("div", { class: "nights", role: "img", "aria-label": [...padded].map((l) => words[l]).join(", ") }, bars),
    el("div", { class: "row2 small muted", style: "justify-content:space-between;margin-top:6px" }, el("span", {}, "older"), el("span", {}, "newest")),
    cell && cell.red_since ? el("p", { class: "small", style: "margin:16px 0 0" }, "First red: " + when(cell.red_since.at) + (cell.red_since.build ? ", build " + cell.red_since.build : ""), " ", el("a", { href: "#run/" + cell.red_since.run }, "that run")) : el("p", { class: "small muted", style: "margin:16px 0 0" }, "Not red in the latest result here."),
    finding && finding.builds.length ? el("p", { class: "small muted", style: "margin:8px 0 0" }, "Builds it failed on: " + finding.builds.join(", ")) : null));
}

/** Run conditions: what the machine and the lab PC were like. */
function conditionsCard(runId, machine, test, result, data) {
  const testLog = logText(result.log ? "/files/" + runId + "/" + result.log : null);
  const healthLine = typeof testLog === "string" ? testLog.split("\n").reverse().find((l) => l.startsWith("Health check after boot")) : null;
  let health = "reading the test log…";
  let clock = "";
  let root = "";
  if (testLog !== undefined) {
    health = healthLine ? healthLine.replace(/^Health check after boot \d+: /, "") : "not checked in this boot: the test shared its boot or the machine has no health check";
    const m = healthLine && healthLine.match(/clock (\d+) s off/);
    const r = healthLine && healthLine.match(/root (\d+) MiB free/);
    clock = m ? "set from the lab PC, " + m[1] + " s off" : "not read";
    root = r ? (Number(r[1]) >= 1024 ? (Number(r[1]) / 1024).toFixed(1) + " GiB" : r[1] + " MiB") + " free" : "not read";
  }
  const loads = result.steps.map((s) => s.lab_pc_load).filter((l) => l != null);
  const load = loads.length ? Math.round(Math.max(...loads) * 100) + "% of its CPUs at most" + (Math.max(...loads) > 0.75 ? ", busy: times may be slow" : ", normal") : "not recorded";
  const others = data ? data.history.filter((p) => p.machine === machine && p.run !== runId && (p.result === "pass" || p.result === "fail")).map((p) => p.milliseconds) : [];
  const usual = median(others);
  const took = duration(result.milliseconds / 1000) + (usual ? ", usual " + duration(usual / 1000) : ", no earlier time here");
  const run = state.runs.find((r) => r.id === runId);
  return card("Run conditions", el("div", { class: "pad" }, el("dl", { class: "facts" },
    el("dt", {}, "Health check"), el("dd", {}, health),
    el("dt", {}, "Clock"), el("dd", {}, clock || "…"),
    el("dt", {}, "Root free"), el("dd", {}, root || "…"),
    el("dt", {}, "Lab PC load"), el("dd", {}, load),
    el("dt", {}, "Build"), el("dd", { class: "mono" }, run && run.build ? run.build : "unnamed"),
    el("dt", {}, "Duration"), el("dd", {}, took),
    el("dt", {}, "Run"), el("dd", {}, el("a", { href: "#run/" + runId }, runId)))));
}

/** The failed steps, the reason, the console's tail and the logs. */
function stepsCard(results, runId, machine, test, result) {
  const file = (path) => "/files/" + runId + "/" + path;
  const also = results.results.filter((r) => r.test === test && r.machine !== machine && (r.result === "fail" || r.result === "error")).map((r) => r.machine);
  const failed = result.steps.filter((s) => s.result !== "pass");
  const rows = failed.map((s, i) => el("tr", { class: i === 0 ? "hot" : "" },
    el("td", {}, s.step),
    el("td", {}, resultWord(s.result === "error" ? "error" : s.result === "skip" ? "skip" : /stalled: /.test(s.note || "") ? "stalled" : "fail")),
    el("td", { class: "mono" }, firstLine(s.note)),
    el("td", {}, also.join(", ") || "—")));
  const table = failed.length
    ? el("div", { class: "scroll" }, el("table", { class: "tbl" },
      el("thead", {}, el("tr", {}, ["Step", "Result", "First line of the error", "Also on"].map((h) => el("th", { scope: "col" }, h)))), el("tbody", {}, rows)))
    : el("p", { class: "pad muted", style: "margin:0" }, result.result === "pass" ? "Every step passed." : "No step is recorded as failed.");
  const serial = result.boot ? file("logs/" + machine + "/serial-" + result.boot + ".log") : null;
  const console_ = serial ? logText(serial) : null;
  const lines = typeof console_ === "string" ? console_.split("\n").slice(-201) : [];
  const hot = /\b(panic|fail(ed)?|error|stalled)\b/i;
  const pre = el("pre", { class: "console", tabindex: "0", role: "region", "aria-label": "Console tail" },
    serial ? (console_ === undefined ? "Reading the serial log…" : lines.length ? lines.map((l) => el("span", { class: hot.test(l) ? "hot" : "" }, l + "\n")) : "The serial log is empty.") : "This result has no boot, so no serial log.");
  const links = [];
  if (serial) links.push(el("a", { href: serial, target: "_blank" }, "Full serial log"));
  if (result.log) links.push(el("a", { href: file(result.log), target: "_blank" }, "Test log"));
  if (result.rerun && result.rerun.log) links.push(el("a", { href: file(result.rerun.log), target: "_blank" }, "Rerun log"));
  links.push(el("a", { href: file("results.json"), target: "_blank" }, "JSON"));
  const shots = result.screenshots.length
    ? el("div", { class: "shots", style: "margin-top:14px" }, result.screenshots.map((s) => el("figure", {}, el("a", { href: file(s), target: "_blank" }, el("img", { src: file(s), alt: "Screenshot " + s.split("/").pop() })), el("figcaption", {}, s.split("/").pop()))))
    : null;
  return el("section", { class: "card", "aria-label": "Failed steps" },
    el("header", {}, el("h2", {}, "Failed steps"), el("span", { class: "small muted" }, machine + ", run " + when(results.run.started))),
    result.reason ? el("pre", { class: "log pre", style: "border-top:0;border-bottom:1px solid var(--line)" }, result.reason.split("\n").slice(0, 12).join("\n")) : null,
    table,
    el("div", { style: "border-top:1px solid var(--line);padding:14px 18px" },
      el("div", { class: "cap" }, el("span", { style: "font-weight:600" }, "Console, last " + Math.min(200, lines.length || 200) + " lines of " + test), el("span", { class: "links" }, links)), pre, shots));
}

/** Notes: the lab keeps no notes store, so only the Not a bug reason is shown. */
function notesCard(finding) {
  return card("Notes", el("div", { class: "pad" },
    el("p", { class: "muted small", style: "margin:0 0 8px" }, "The lab keeps no notes of its own yet. The only note a failure carries is the reason a person gave when they said it is not a bug."),
    finding && finding.state === "not-a-bug"
      ? el("p", { style: "margin:0" }, el("b", {}, "Not a bug: "), finding.note || "no reason given")
      : el("p", { class: "muted", style: "margin:0" }, "No reason is recorded for this failure.")));
}

// ---- Findings ----

/** The class a finding has in the grid, if its test is in it. */
function findingClass(finding) {
  if (finding.class) return finding.class;
  if (state.grid) {
    for (const row of state.grid.rows) {
      for (const cell of Object.values(row.cells)) if (cell.finding === finding.id && cell.class) return cell.class;
    }
  }
  if (finding.state === "not-a-bug") return "test";
  return finding.fix || ["fix-proposed", "approved", "fixed"].includes(finding.state) ? "known" : "dukovo";
}

function findingsView() {
  const canSend = state.page && state.page.can_send;
  const toSend = state.findings.filter(sendable);
  const rows = state.findings.map((finding) => {
    const last = finding.runs[finding.runs.length - 1];
    return el("tr", {},
      el("th", { scope: "row" }, el("a", { class: "plain", href: failureHash(last.run, last.machine, finding.test) }, finding.test), el("small", {}, firstLine(finding.signature))),
      el("td", {}, classChip(findingClass(finding))),
      el("td", {}, el("span", { class: "chip " + (finding.state === "new" ? "dukovo" : finding.state === "fixed" ? "pass" : finding.state === "not-a-bug" ? "plain" : "run") }, STATE_WORDS[finding.state])),
      el("td", {}, String(finding.count)), el("td", {}, finding.machines.join(", ")),
      el("td", {}, when(finding.last_seen)),
      el("td", {}, finding.state === "not-a-bug" ? "—" : unsent(finding) ? "not sent" : "sent"));
  });
  const note = !canSend
    ? "Send needs the lab-results clone at " + ((state.page && state.page.results) || "lab-results in the store") + ": ob lab makes it, or see docs/sending.md."
    : toSend.length ? "Sends the findings, their runs and logs to the project's private lab-results repository. A thread proposes a fix for each, and nothing is fixed until you approve it." : "Everything here is in lab-results, or marked not a bug.";
  return el("div", { style: "display:flex;flex-direction:column;gap:20px" }, el("h1", { class: "vh" }, "Findings"),
    el("section", { class: "card", "aria-label": "Findings" },
      el("header", {}, el("h2", {}, "Findings"), el("span", { class: "small muted" }, plural(state.findings.length, "finding")),
        el("button", { class: "btn ink grow", type: "button", disabled: !canSend || state.sending || !toSend.length, onclick: () => sendNow({ all: true }), "data-act": true },
          state.sending ? "Sending…" : "Send all new (" + toSend.length + ")")),
      state.findings.length
        ? el("div", { class: "scroll" }, el("table", { class: "tbl" },
          el("thead", {}, el("tr", {}, ["Test and how it fails", "Class", "State", "Times", "Machines", "Last seen", "Sent"].map((h) => el("th", { scope: "col" }, h)))), el("tbody", {}, rows)))
        : el("p", { class: "pad muted", style: "margin:0" }, "No test has failed yet."),
      el("p", { class: "pad small muted", style: "margin:0;border-top:1px solid var(--line)" }, note)));
}

// ---- Fleet: machines, health, schedules and who is waiting ----

function capChips(m) {
  const person = Object.entries(m.controls).filter(([, who]) => who === "person").map(([c]) => c);
  const none = Object.entries(m.controls).filter(([, who]) => who === "none").map(([c]) => c);
  const driver = Object.entries(m.controls).filter(([, who]) => who === "driver").map(([c]) => c);
  const shown = driver.slice(0, 6);
  return el("div", { class: "caps" },
    shown.map((c) => el("span", {}, c)), driver.length > shown.length ? el("span", { title: driver.slice(6).join(", ") }, "+" + (driver.length - shown.length) + " more") : null,
    person.map((c) => el("span", { class: "person" }, "you: " + c)), none.map((c) => el("span", { class: "none" }, "no " + c)));
}

function machineCard(m) {
  let badge = ["idle", "plain"];
  let doing = m.spare ? "A spare: kept for reruns and bisects, and left out of a run on its pool." : "Idle.";
  if (m.benched) {
    badge = ["benched", "lab"];
    doing = m.benched.reason + " (run " + when(m.benched.at) + ")";
  } else if (m.doing) {
    badge = ["running", "run"];
    const d = m.doing;
    doing = (d.test || "starting") + (d.total != null ? " · " + d.done + " of " + d.total + " tests" : d.done ? " · " + d.done + " tests ended" : "");
  } else if (m.spare) {
    badge = ["spare", "plain"];
  }
  const checks = m.health
    ? el("ul", { class: "checks", "aria-label": m.name + " health checks" }, m.health.checks.map((c) => el("li", { class: c.ok ? "ok" : "bad" }, (c.ok ? "✓ " : "✕ ") + c.label + ": " + c.detail.replace(/^(root|clock) /, ""))),
      el("li", { class: "muted", style: "grid-column:1/-1" }, "checked " + when(m.health.at) + (m.health.ok ? "" : ", failed")))
    : el("p", { class: "muted small", style: "margin:12px 0 0" }, m.driver === "qemu" ? "QEMU has no health check." : "No run has booted it yet.");
  return el("article", { class: "card mcard" + (m.benched ? " benched" : "") },
    el("div", { class: "top" }, el("h3", {}, m.name), el("span", { class: "chip " + badge[1] }, badge[0])),
    el("div", { class: "small muted" }, m.about), el("div", { class: "small", style: "margin-top:4px" }, doing), checks, capChips(m));
}

function fleetView() {
  const fleet = state.fleet;
  if (!fleet) return el("p", { class: "muted" }, "Reading the machines…");
  const pools = [];
  for (const m of fleet.machines) if (m.pool && !pools.includes(m.pool)) pools.push(m.pool);
  const sections = pools.map((pool) => {
    const members = fleet.machines.filter((m) => m.pool === pool);
    const running = members.filter((m) => m.doing && !m.spare).length;
    const spares = members.filter((m) => m.spare).length;
    return el("section", { "aria-label": "Pool " + pool },
      el("div", { class: "row2", style: "align-items:baseline;margin-bottom:12px" }, el("h2", { style: "font-size:18px" }, "Pool · " + pool),
        el("span", { class: "small muted" }, plural(members.length - spares, "machine") + ", " + running + " running" + (spares ? ", " + plural(spares, "spare") + " for reruns" : "") + ". A run on pool:" + pool + " shares its tests among the machines that are not benched.")),
      el("div", { class: "fleet" }, members.map(machineCard)));
  });
  const others = fleet.machines.filter((m) => !m.pool);
  return el("div", { style: "display:flex;flex-direction:column;gap:24px" }, el("h1", { class: "vh" }, "Fleet"),
    sections,
    others.length ? el("section", { "aria-label": "Other machines" }, el("h2", { style: "font-size:18px;margin-bottom:12px" }, "Other machines"), el("div", { class: "fleet" }, others.map(machineCard))) : null,
    timelineCard(), schedulesCard(), waitingCard(fleet));
}

/** "Waiting for a person": the steps runs going now wait on, with the Done flow. */
function waitingCard(fleet) {
  const waits = fleet.waiting.map((w) => el("div", { class: "wait" },
    el("span", {}, el("strong", {}, "Needs you on " + w.machine + ": "), w.step + " (" + w.test + "), then press Done."),
    el("button", { class: "btn ink", type: "button", "data-act": true, onclick: () => act("/api/done", { run: w.run, machine: w.machine }).then(refresh) }, "Done")));
  return card("Waiting for a person" + (waits.length ? " · " + plural(waits.length, "step") : ""), el("div", { class: "pad", style: "display:flex;flex-direction:column;gap:10px" },
    waits.length ? waits : el("p", { class: "muted", style: "margin:0" }, "No run is waiting for a person. Tests that need one run only when you pick them in Start run; the run then waits here, and on its own page, with a Done button.")));
}

/** HH:MM as minutes into the day. */
function minutesOf(at) {
  const [h, m] = at.split(":").map(Number);
  return h * 60 + m;
}

/** The schedules as a 24-hour timeline, in the lab's zone. */
function timelineCard() {
  const listed = state.schedules;
  if (!listed) return null;
  const timed = (listed.schedules || []).filter((s) => !s.each_build && s.at);
  const build = (listed.schedules || []).filter((s) => s.each_build);
  const hours = Array.from({ length: 24 }, (_, h) => (h === 0 ? "12a" : h < 12 ? h + "a" : h === 12 ? "12p" : h - 12 + "p"));
  const lanes = new Map();
  for (const s of timed) {
    for (const machine of s.machines) {
      if (!lanes.has(machine)) lanes.set(machine, []);
      lanes.get(machine).push(s);
    }
  }
  const laneNodes = [...lanes.entries()].map(([machine, list]) => el("div", { class: "lane" }, el("b", {}, machine.replace(/^pool:/, "pool ")),
    el("div", { class: "area" }, list.map((s) => {
      const start = minutesOf(s.at);
      const minutes = s.last && s.last.started && s.last.ended ? Math.max(30, (new Date(s.last.ended) - new Date(s.last.started)) / 60000) : 60;
      const width = Math.min(minutes, 1440 - start);
      const days = s.days && s.days.length ? s.days.join(", ") : "every day";
      const block = el("div", { class: "block" + (s.paused ? " paused" : ""), title: scheduleTitle(s.name) + ", " + s.at + " on " + days + (s.paused ? " (paused)" : ""), role: "img", "aria-label": scheduleTitle(s.name) + " at " + s.at + " on " + days }, s.at);
      block.style.left = (100 * start) / 1440 + "%";
      block.style.width = "calc(" + (100 * width) / 1440 + "% - 3px)";
      return block;
    }))));
  const zone = listed.zone_short ? listed.zone_short + ", " + listed.zone : listed.zone || "America/Denver";
  return card("Schedules, over 24 hours", el("div", {}, el("div", { class: "pad scroll" },
    laneNodes.length
      ? el("div", { class: "timeline" }, el("div", { class: "hours" }, el("span"), el("div", { class: "scale" }, hours.map((h) => el("span", {}, h)))), laneNodes)
      : el("p", { class: "muted", style: "margin:0" }, "No schedule runs at a set time."),
    ), el("p", { class: "pad small muted", style: "margin:0;border-top:1px solid var(--line)" },
    "Times are " + zone + ", whatever zone this PC is in. A block is as long as the schedule's last run took, or an hour. The lab PC's timer starts each one, so closing this page does not stop a run." +
      (build.length ? " On each new build on main: " + build.map((s) => scheduleTitle(s.name)).join(", ") + "." : ""))));
}

// ---- Schedules ----

const DAYS = [["mon", "Mon"], ["tue", "Tue"], ["wed", "Wed"], ["thu", "Thu"], ["fri", "Fri"], ["sat", "Sat"], ["sun", "Sun"]];

/** What a schedule is called: its title, its preset's, or its name. */
function scheduleTitle(name) {
  const schedule = ((state.schedules && state.schedules.schedules) || []).find((s) => s.name === name);
  if (!schedule) return name;
  const preset = state.presets.find((p) => p.name === schedule.preset);
  return schedule.title || (preset ? preset.title : schedule.name);
}

function schedulesCard() {
  const listed = state.schedules;
  if (!listed) return null;
  const schedules = listed.schedules || [];
  const rows = schedules.map((schedule) => {
    let next;
    if (schedule.going) next = el("span", { class: "chip run" }, schedule.going.run ? "Running" : "Starting");
    else if (schedule.paused) next = "Paused";
    else if (schedule.each_build) next = "On the next build on main";
    else next = schedule.next_text || "";
    const title = el("th", { scope: "row" }, scheduleTitle(schedule.name));
    const line = schedule.going ? schedule.going.doing : lastText(schedule.last);
    if (line) title.append(el("small", {}, line));
    return el("tr", {}, title, el("td", {}, schedule.when), el("td", {}, schedule.machines.join(", ")),
      el("td", {}, schedule.send ? "Send when done" : "Hold for me"), el("td", {}, next),
      el("td", {}, el("button", { class: "btn mini", type: "button", "data-act": true, onclick: () => openForm(schedule) }, "Edit")));
  });
  let note = "Scheduled runs always skip tests that need you. A scheduled run waits if a run is already going on that machine, and tests main as it is then, on the lab's own Dukovo checkout.";
  if (listed.zone_problem) note += " Times cannot be worked out: " + listed.zone_problem;
  if (!listed.timer) note += " The timer that starts schedules is not installed yet: ob lab installs it.";
  return el("section", { class: "card", "aria-label": "Schedules" },
    el("header", {}, el("h2", {}, "Schedules"), el("button", { class: "btn grow", type: "button", "data-act": true, onclick: () => openForm(null) }, "New schedule")),
    schedules.length
      ? el("div", { class: "scroll" }, el("table", { class: "tbl" },
        el("thead", {}, el("tr", {}, ["Runs", "When (" + (listed.zone_short || listed.zone) + ")", "Machines", "Findings", "Next", ""].map((h) => el("th", { scope: "col" }, h)))), el("tbody", {}, rows)))
      : state.editing ? null : el("p", { class: "pad muted", style: "margin:0" }, "No schedules yet. New schedule makes one: tests that run by themselves at a time or on each new build on main, with this page closed."),
    state.editing ? scheduleForm() : null,
    el("p", { class: "pad small muted", style: "margin:0;border-top:1px solid var(--line)" }, note));
}

/** How a schedule's last start went, in a few words. */
function lastText(last) {
  if (!last) return "";
  const words = { passed: "passed", failed: "failed", broke: "the lab broke", stopped: "stopped", missed: "missed", error: "could not run" };
  let text = "Last: " + when(last.started) + ", " + (words[last.result] || last.result);
  const count = (last.findings || []).length;
  if (count) text += ", " + plural(count, "finding") + (last.sent ? " sent" : " held");
  if (last.note) text += " (" + last.note + ")";
  return text;
}

/** Opens the schedule form, for a new schedule or to change one. */
function openForm(schedule) {
  notice("");
  const firstPreset = state.presets[0];
  let what;
  if (schedule) what = schedule.preset ? "preset:" + schedule.preset : schedule.all ? "all" : "own";
  else if (state.preset) what = "preset:" + state.preset;
  else if (state.chosen.size) what = "picked";
  else what = firstPreset ? "preset:" + firstPreset.name : "all";
  state.editing = {
    isNew: !schedule,
    name: schedule ? schedule.name : "",
    title: (schedule && schedule.title) || "",
    what,
    tests: schedule ? schedule.tests || [] : [],
    machines: new Set(schedule ? schedule.machines : state.picked),
    kind: schedule && schedule.each_build ? "build" : "time",
    at: (schedule && schedule.at) || "01:00",
    days: new Set(schedule && schedule.days && schedule.days.length ? schedule.days : DAYS.map(([d]) => d)),
    send: schedule ? schedule.send : true,
    paused: schedule ? !!schedule.paused : false,
    removing: false,
  };
  render();
  byId("schedule-form") && byId("schedule-form").scrollIntoView({ block: "nearest" });
}

function closeForm() {
  state.editing = null;
  render();
}

/** The form, drawn from state.editing: what is typed lives there, so a redraw keeps it. */
function scheduleForm() {
  const f = state.editing;
  const row = (label, ...content) => el("div", { class: "row" }, el("span", {}, label), el("div", { class: "opts" }, ...content));
  const radio = (name, value, checked, label, onpick) => el("label", {}, el("input", { type: "radio", name, value, checked, onchange: onpick }), label);
  const box = (checked, label, onflip) => el("label", {}, el("input", { type: "checkbox", checked, onchange: onflip }), label);
  const name = el("input", { type: "text", id: "sched-name", value: f.name, placeholder: "nightly", disabled: !f.isNew, "aria-label": "Schedule name", oninput: (e) => (f.name = e.target.value.trim()) });
  const title = el("input", { type: "text", id: "sched-title", value: f.title, placeholder: "Full run", maxlength: "80", "aria-label": "What to call it", oninput: (e) => (f.title = e.target.value) });
  const options = state.presets.map((p) => el("option", { value: "preset:" + p.name, selected: f.what === "preset:" + p.name }, p.title));
  options.push(el("option", { value: "picked", selected: f.what === "picked" }, "The tests picked in Start run (" + state.chosen.size + ")"));
  options.push(el("option", { value: "all", selected: f.what === "all" }, "Every test"));
  if (f.what === "own") options.push(el("option", { value: "own", selected: true }, "Its own tests (" + f.tests.length + ")"));
  const what = el("select", { id: "sched-what", "aria-label": "What runs", onchange: (e) => (f.what = e.target.value) }, options);
  const machines = columns().map((c) => box(f.machines.has(c.name), c.name, () => {
    f.machines.has(c.name) ? f.machines.delete(c.name) : f.machines.add(c.name);
  }));
  const time = el("input", { type: "time", id: "sched-time", value: f.at, "aria-label": "Time", disabled: f.kind !== "time", oninput: (e) => (f.at = e.target.value) });
  const days = DAYS.map(([d, label]) => box(f.days.has(d), label, () => (f.days.has(d) ? f.days.delete(d) : f.days.add(d))));
  const pick = (kind) => () => {
    f.kind = kind;
    render();
  };
  const zone = (state.schedules && (state.schedules.zone_short || state.schedules.zone)) || "";
  const buttons = [el("button", { class: "btn ink", type: "submit" }, f.isNew ? "Make schedule" : "Save"), el("button", { class: "btn", type: "button", onclick: closeForm }, "Cancel")];
  const others = [];
  if (!f.isNew) {
    others.push(el("button", { class: "btn", type: "button", onclick: () => scheduleAction({ start: f.name }) }, "Run now"));
    others.push(el("button", { class: "btn", type: "button", onclick: () => scheduleAction(f.paused ? { resume: f.name } : { pause: f.name }) }, f.paused ? "Resume" : "Pause"));
    others.push(el("button", { class: "btn danger", type: "button", onclick: () => {
      if (!f.removing) {
        f.removing = true;
        render();
        return;
      }
      scheduleAction({ remove: f.name });
    } }, f.removing ? "Remove it for good" : "Remove"));
  }
  const form = el("form", { class: "sform", id: "schedule-form", onsubmit: (event) => {
    event.preventDefault();
    saveSchedule();
  } },
    el("h3", {}, f.isNew ? "New schedule" : "Change " + f.name),
    row("Name", name), row("Call it", title), row("Runs", what), row("On", ...machines),
    row("When", radio("kind", "time", f.kind === "time", "At", pick("time")), time, el("span", { class: "muted small" }, zone)),
    row("", ...days),
    row("", radio("kind", "build", f.kind === "build", "On each new build on main", pick("build"))),
    row("Findings", radio("send", "send", f.send, "Send when done", () => (f.send = true)), radio("send", "hold", !f.send, "Hold for me", () => (f.send = false))),
    el("div", { class: "acts" }, el("div", {}, ...buttons), el("div", {}, ...others)));
  return form;
}

/** Saves the form as a schedule. */
async function saveSchedule() {
  notice("");
  const f = state.editing;
  const save = { name: f.name, title: f.title, machines: [...f.machines], send: f.send };
  if (f.what.startsWith("preset:")) save.preset = f.what.slice(7);
  else if (f.what === "all") save.all = true;
  else save.tests = f.what === "own" ? f.tests : [...state.chosen];
  if (f.kind === "build") save.each_build = true;
  else {
    save.at = f.at;
    // Every day is said by naming none.
    save.days = f.days.size === DAYS.length ? [] : DAYS.map(([d]) => d).filter((d) => f.days.has(d));
    if (!save.days.length && f.days.size === 0) return notice("Pick the days it runs on.");
  }
  const answer = await act("/api/schedules", { save });
  if (answer.error) return;
  closeForm();
  await refresh();
}

/** Starts, pauses, resumes or removes a schedule. */
async function scheduleAction(body) {
  notice("");
  const answer = await act("/api/schedules", body);
  if (answer.error) return;
  closeForm();
  if (body.start) notice("Schedule " + body.start + " started. It shows under Past runs once its image is built.");
  await refresh();
}

// ---- Past runs ----

/** A past run's findings, and whether they went to lab-results. */
function runFindings(run) {
  if (run.state === "running") return "";
  const found = state.findings.filter((f) => f.runs.some((s) => s.run === run.id));
  if (!found.length) return run.sent ? "Sent" : "Nothing to send";
  let text = plural(found.length, "finding");
  if (found.every((f) => f.state === "fixed")) text += ", fixed";
  else if (found.every((f) => f.state === "not-a-bug")) text += ", not bugs";
  else text += run.sent ? ", sent" : found.some(sendable) ? ", not sent" : "";
  return text;
}

function runName(run) {
  const preset = state.presets.find((p) => p.name === run.preset);
  if (run.schedule) return scheduleTitle(run.schedule) + " (scheduled)";
  return preset ? preset.title : plural(run.tests, "test");
}

function runResultNode(run) {
  const s = run.summary;
  if (run.state === "running") return el("span", { class: "chip run" }, "▸ Running");
  if (run.state === "died") return el("span", { class: "chip dukovo" }, "✕ Died");
  if (run.state === "stopped") return el("span", { class: "chip plain" }, "Stopped · " + s.passed + " passed");
  return el("span", { class: "chip " + (s.failed || s.errors ? "dukovo" : "pass") }, (s.failed || s.errors ? "✕ " : "✓ ") + s.passed + " of " + (s.passed + s.failed + s.errors));
}

function runsView() {
  const rows = state.runs.map((run) => el("tr", {},
    el("td", {}, el("a", { href: "#run/" + run.id }, when(run.started))), el("td", {}, runName(run)), el("td", {}, run.machines.join(", ")),
    el("td", { class: "mono" }, run.build || ""), el("td", {}, runResultNode(run)), el("td", {}, runFindings(run))));
  return el("div", { style: "display:flex;flex-direction:column;gap:20px" }, el("h1", { class: "vh" }, "Past runs"),
    el("section", { class: "card", "aria-label": "Past runs" },
      el("header", {}, el("h2", {}, "Past runs"), el("a", { class: "btn grow", href: "#start", "data-act": true }, "Start run")),
      state.runs.length
        ? el("div", { class: "scroll" }, el("table", { class: "tbl" },
          el("thead", {}, el("tr", {}, ["When", "Run", "Machines", "Build", "Result", "Findings"].map((h) => el("th", { scope: "col" }, h)))), el("tbody", {}, rows)))
        : el("p", { class: "pad muted", style: "margin:0" }, "No runs yet.")));
}

/** One event as a line of the live log. */
function logLine(event) {
  const time = event.at ? new Date(event.at).toLocaleTimeString([], { hour12: false }) : "";
  const where = event.machine ? event.machine + " · " + (event.test || "") : "run";
  let text;
  let bad = false;
  switch (event.event) {
    case "run-started": text = "started " + plural(event.tests.length, "test") + " on " + event.machines.join(", "); break;
    case "test-started": text = "started"; break;
    case "step-ended":
      bad = event.result === "fail" || event.result === "error";
      text = event.step + ": " + (event.result === "pass" ? "ok" : event.result.toUpperCase()) + " (" + duration(event.milliseconds / 1000) + ")";
      break;
    case "person-needed": text = "NEEDS YOU: " + event.step; break;
    case "test-ended": {
      bad = event.result === "fail" || event.result === "error";
      const word = { pass: "PASSED", fail: "FAILED", skip: "SKIPPED", error: "ERROR" }[event.result] || event.result;
      text = word + (event.reason ? ": " + firstLine(event.reason) : "");
      break;
    }
    case "rerun-started": text = "running again"; break;
    case "rerun-ended": text = "run again on " + event.on + ": " + (event.result === "pass" ? "passed, so it is flaky" : "failed again"); bad = event.result !== "pass"; break;
    case "machine-out": text = "out of its pool: " + event.reason; bad = true; break;
    case "run-ended": {
      const found = (event.findings || []).length;
      text = (event.stopped ? "stopped" : "ended") + (found ? " · " + plural(found, "finding") : "");
      break;
    }
    default: return null;
  }
  return el("div", {}, el("span", { class: "t" }, time), " ", el("span", { class: "k" }, where), " ", el("span", { class: bad ? "e" : "" }, text));
}

/** One run: its progress, what waits for a person, its results and its log. */
function runView() {
  const v = state.runView;
  const crumb = el("a", { class: "crumb", href: "#runs" }, "← Past runs");
  if (!v || v.id !== state.route.args[0]) return el("div", {}, crumb, el("p", { class: "muted" }, "Reading the run…"));
  const { status, results } = v;
  const word = { running: "going now", ended: "", stopped: "stopped", died: "its process ended" }[status.state];
  const c = status.counts;
  const total = c.passed + c.failed + c.skipped + c.errors + c.running + c.queued || 1;
  const bar = (cls, n) => {
    const part = el("i", { class: cls });
    part.style.width = (100 * n) / total + "%";
    return part;
  };
  const legend = [c.passed + " passed", c.failed + " failed"];
  if (c.errors) legend.push(plural(c.errors, "lab fault"));
  if (c.skipped) legend.push(c.skipped + " skipped");
  if (status.state === "running") {
    legend.push(c.running + " running", c.queued + " queued");
    if (status.left_seconds != null) legend.push("about " + duration(status.left_seconds) + " left");
  }
  const machines = status.machines.map((m) => el("span", {}, el("strong", {}, m.name), " " + m.counts.passed + " passed, " + (m.counts.failed + m.counts.errors) + " failed" + (m.running ? ", running " + m.running : "")));
  const waits = status.waiting.map((wait) => el("div", { class: "wait" },
    el("span", {}, el("strong", {}, "Needs you on " + wait.machine + ": "), wait.step + " (" + wait.test + "), then press Done."),
    el("button", { class: "btn ink", type: "button", "data-act": true, onclick: () => act("/api/done", { run: status.id, machine: wait.machine }).then(refresh) }, "Done")));
  const stop = status.state === "running"
    ? el("button", { class: "btn grow", type: "button", "data-act": true, disabled: state.stopping, onclick: async () => {
      state.stopping = true;
      render();
      await act("/api/stop", { run: status.id });
      state.stopping = false;
      await refresh();
    } }, state.stopping ? "Stopping…" : "Stop")
    : null;
  const red = results.results.filter((r) => r.result === "fail" || r.result === "error");
  const skipped = results.results.filter((r) => r.result === "skip");
  const classOf = (r) => (results.classes.find((x) => x.machine === r.machine && x.test === r.test) || {}).class;
  const redRows = red.map((r) => el("tr", { class: "hot" }, el("th", { scope: "row" }, el("a", { class: "plain", href: failureHash(status.id, r.machine, r.test) }, r.test)),
    el("td", {}, r.machine), el("td", {}, classChip(classOf(r))),
    el("td", {}, resultWord(r.result === "error" ? "error" : r.rerun && r.rerun.result === "pass" ? "flaky" : /: stalled: /.test(firstLine(r.reason)) ? "stalled" : "fail")),
    el("td", { class: "small" }, firstLine(r.reason))));
  const skipRows = skipped.map((r) => el("tr", {}, el("th", { scope: "row" }, r.test), el("td", {}, r.machine), el("td", { class: "small muted" }, firstLine(r.reason))));
  const ofRun = state.findings.filter((f) => f.runs.some((s) => s.run === status.id));
  const toSend = ofRun.filter(sendable);
  const canSend = state.page && state.page.can_send;
  const log = el("pre", { class: "log", tabindex: "0", role: "log", "aria-label": "Live log", id: "run-log" }, status.recent.map(logLine).filter(Boolean));
  const meta = [["Started", when(status.started) + (status.ended ? ", ended " + when(status.ended) : "")], ["Build", status.build || "unnamed"]];
  if (status.preset) meta.push(["Preset", status.preset]);
  if (status.schedule) meta.push(["Schedule", scheduleTitle(status.schedule)]);
  meta.push(["Bundle", status.bundle]);
  return el("div", { style: "display:flex;flex-direction:column;gap:20px" }, crumb,
    el("div", { class: "row2" }, el("h1", { style: "font-size:24px" }, "Run " + when(status.started) + (word ? ", " + word : "")), stop),
    waits.length ? el("div", { style: "display:flex;flex-direction:column;gap:10px" }, waits) : null,
    el("section", { class: "card", "aria-label": "Progress" }, el("div", { class: "pad", style: "display:grid;gap:10px" },
      el("div", { class: "track big", role: "img", "aria-label": legend.join(", ") }, bar("t-pass", c.passed), bar("t-fail", c.failed + c.errors), bar("t-skip", c.skipped), bar("t-run", c.running)),
      el("div", { class: "row2 small muted", style: "gap:6px 14px" }, legend.map((t) => el("span", {}, t))),
      el("div", { class: "row2 small", style: "gap:6px 18px" }, machines),
      el("dl", { class: "facts" }, meta.flatMap(([k, val]) => [el("dt", {}, k), el("dd", { class: k === "Bundle" ? "mono" : "" }, val)])),
      status.output ? el("pre", { class: "log", style: "border:1px solid var(--line);border-radius:6px" }, status.output) : null)),
    el("section", { class: "card", "aria-label": "What failed" },
      el("header", {}, el("h2", {}, "Failed and lab faults"), el("span", { class: "small muted" }, red.length + " of " + results.results.length + " results"),
        toSend.length || ofRun.length ? el("button", { class: "btn ink grow", type: "button", "data-act": true, disabled: !canSend || state.sending || !toSend.length, title: canSend ? null : "Send needs a lab-results clone: ob lab makes it",
          onclick: () => sendNow({ run: status.id }) }, state.sending ? "Sending…" : toSend.length ? "Send " + plural(toSend.length, "finding") : "Sent") : null),
      red.length
        ? el("div", { class: "scroll" }, el("table", { class: "tbl" },
          el("thead", {}, el("tr", {}, ["Test", "Machine", "Class", "Result", "Why"].map((h) => el("th", { scope: "col" }, h)))), el("tbody", {}, redRows)))
        : el("p", { class: "pad muted", style: "margin:0" }, status.state === "running" ? "Nothing has failed so far." : "Nothing failed.")),
    skipped.length
      ? el("section", { class: "card", "aria-label": "Skipped" }, el("details", {}, el("summary", { class: "pad", style: "cursor:pointer" }, plural(skipped.length, "skipped result")),
        el("div", { class: "scroll" }, el("table", { class: "tbl" }, el("tbody", {}, skipRows)))))
      : null,
    el("section", { class: "card", "aria-label": "Log" }, el("header", {}, el("h2", {}, "Live log")), log));
}

// ---- Start a run ----

function startView() {
  if (readOnly()) return el("div", { style: "display:flex;flex-direction:column;gap:20px" }, el("h1", { style: "font-size:24px" }, "Start a run"), el("p", { class: "notice" }, "This address is read only: runs are started from the page on the lab PC."));
  const can = state.page && state.page.can_run;
  const find = state.startFind.toLowerCase();
  const shown = state.tests.filter((t) => !find || t.id.includes(find) || t.about.toLowerCase().includes(find));
  const needs = shown.filter(needsPerson);
  const groups = state.categories.map((category) => ({
    title: category.title,
    tests: shown.filter((t) => t.category === category.id && !needsPerson(t)),
    all: state.tests.filter((t) => t.category === category.id),
  }));
  const nodes = groups.map((g) => startCategory(g.title, g.tests, g.all, false));
  if (needs.length) nodes.push(startCategory("Needs you", needs, needs, true));
  const chosen = state.tests.filter((t) => state.chosen.has(t.id));
  const machines = columns().map((column) => {
    const on = state.picked.has(column.name);
    const machine = machineOfColumn(column.name);
    const byPerson = machine ? Object.entries(machine.controls).filter(([, who]) => who === "person").map(([c]) => c) : [];
    const about = column.kind === "pool" ? "a pool of " + plural(column.members.length, "machine") + ": its tests are shared out among them" : machine ? machine.about : "";
    const box = el("input", { type: "checkbox", checked: on, id: "pick-" + column.name, onchange: () => {
      on ? state.picked.delete(column.name) : state.picked.add(column.name);
      render();
    } });
    const busy = state.runs.some((r) => r.state === "running" && r.machines.some((m) => columnOf(m) === column.name));
    return el("label", { class: on ? "on" : "", for: "pick-" + column.name }, box,
      el("span", {}, el("b", {}, column.name), busy ? el("span", { class: "chip run", style: "margin-left:8px" }, "running") : null,
        el("span", { class: "small muted", style: "display:block" }, about + (byPerson.length ? " · you do: " + byPerson.join(", ") : ""))));
  });
  const options = [el("option", { value: "" }, "Picked by hand")];
  for (const preset of state.presets) {
    const seconds = typicalSeconds(preset.tests);
    options.push(el("option", { value: preset.name, selected: preset.name === state.preset }, preset.title + (seconds != null ? " (about " + duration(seconds) + ")" : "")));
  }
  const preset = state.presets.find((p) => p.name === state.preset);
  const run = el("button", { class: "btn ink", type: "button", disabled: !can || !(preset || chosen.length) || !state.picked.size, onclick: () => startRun(null, null) },
    preset ? "Run " + preset.title : "Run " + chosen.length + " picked");
  return el("div", { style: "display:flex;flex-direction:column;gap:20px" }, el("h1", { style: "font-size:24px" }, "Start a run"),
    can === false ? el("p", { class: "notice" }, "This page was opened without an image, so it can show runs but not start them. Open it with ob lab ui.") : null,
    card("Machines", el("div", {}, el("p", { class: "pad small muted", style: "margin:0;padding-bottom:0" }, "Pick one or more. Each machine runs every test picked, at the same time; a pool shares them out among its machines."), el("div", { class: "pick" }, machines))),
    el("section", { class: "card", "aria-label": "Tests" },
      el("header", {}, el("h2", {}, "Tests"), el("span", { class: "small muted" }, state.tests.length + " tests · " + chosen.length + " picked"),
        el("div", { class: "row2 grow" },
          el("label", { class: "vh", for: "start-find" }, "Find a test"),
          el("input", { type: "search", id: "start-find", value: state.startFind, placeholder: "Find a test", oninput: (e) => {
            state.startFind = e.target.value;
            render();
          } }),
          el("label", { class: "vh", for: "start-preset" }, "Preset"),
          el("select", { id: "start-preset", onchange: (e) => {
            state.preset = e.target.value;
            const p = state.presets.find((x) => x.name === state.preset);
            if (p) state.chosen = new Set(p.tests);
            render();
          } }, options), run)),
      nodes));
}

/** One category of tests, or the Needs you group, as a group to tick. */
function startCategory(title, tests, all, needs) {
  const ids = all.map((t) => t.id);
  const picked = ids.filter((id) => state.chosen.has(id)).length;
  const can = state.page && state.page.can_run && state.picked.size;
  const box = el("input", { type: "checkbox", checked: ids.length > 0 && picked === ids.length, disabled: !ids.length, "aria-label": "All " + title + " tests",
    // Inside a summary a click would also open or close the group.
    onclick: (event) => {
      event.preventDefault();
      const every = picked === ids.length;
      ids.forEach((id) => (every ? state.chosen.delete(id) : state.chosen.add(id)));
      state.preset = "";
      render();
    } });
  const runAll = el("button", { class: "btn mini", type: "button", disabled: !ids.length || !can, onclick: (event) => {
    event.preventDefault();
    startRun(ids, null);
  } }, "Run all " + ids.length);
  const open = state.openGroups && state.openGroups.has(title);
  const group = el("details", { class: "cat" + (needs ? " needs" : ""), open: open || needs, ontoggle: (e) => {
    if (!state.openGroups) state.openGroups = new Set();
    e.target.open ? state.openGroups.add(title) : state.openGroups.delete(title);
  } },
    el("summary", {}, box, el("b", {}, title), el("span", { class: "small muted" }, ids.length ? picked + " of " + ids.length : "no tests yet"), runAll),
    el("ul", {}, tests.map((test) => el("li", {},
      el("input", { type: "checkbox", id: "t-" + test.id, checked: state.chosen.has(test.id), onchange: () => {
        state.chosen.has(test.id) ? state.chosen.delete(test.id) : state.chosen.add(test.id);
        state.preset = "";
        render();
      } }),
      el("label", { for: "t-" + test.id }, el("a", { href: testHash(test.id), class: "mono" }, test.id), el("span", { class: "small muted", style: "display:block" }, test.about)),
      el("button", { class: "btn mini", type: "button", disabled: !can, onclick: () => startRun([test.id], null) }, "Run")))));
  if (needs) group.append(el("p", { class: "pad small", style: "margin:0" }, "These ask you to do a step, such as pulling a cable, and wait until you press Done. They run only when ticked."));
  return group;
}

// ---- Acting ----

/** Sends to lab-results, then shows what is sent. */
async function sendNow(body) {
  notice("");
  state.sending = true;
  render();
  try {
    await act("/api/send", body);
  } finally {
    state.sending = false;
    await reloadFindings();
  }
}

/** Says a finding is not a bug, or reopens it, or gives it a class. */
async function changeFinding(body) {
  notice("");
  const answer = await act("/api/findings", body);
  if (!answer.error) state.noting.delete(body.finding);
  await reloadFindings();
}

/** Reads the findings, and the grid and results whose classes they change, again. */
async function reloadFindings() {
  state.findings = (await read("/api/findings")).findings || [];
  state.grid = null;
  state.failure = state.failure && { ...state.failure, runsKey: "" };
  await refresh(true);
}

/** Starts a run of `tests` (or the chosen preset when tests is null and one is chosen). */
async function startRun(tests, machines) {
  notice("");
  const body = { machines: machines || [...state.picked] };
  const preset = tests === null ? state.presets.find((p) => p.name === state.preset) : null;
  if (preset) body.preset = preset.name;
  else body.tests = tests || [...state.chosen];
  // Needs you tests in the run, a preset's too, run with a person: they
  // are ticked, and the page says ticked ones run.
  body.with_person = (body.tests || preset.tests).some((id) => {
    const test = state.tests.find((t) => t.id === id);
    return test && body.machines.some((column) => fit(test, column) === "needs-person");
  });
  const answer = await act("/api/run", body);
  if (answer.id) {
    state.runView = null;
    location.hash = "#run/" + answer.id;
    await refresh(true);
  }
}

// ---- Keeping up ----

/** Keeps `value` under `key` and says whether it changed. */
const seen = new Map();
function put(key, value) {
  const text = JSON.stringify(value);
  const changed = seen.get(key) !== text;
  seen.set(key, text);
  state[key] = value;
  return changed;
}

/** Reads what changes while the page is open, and draws again when something did. */
async function refresh(force) {
  let changed = !!force;
  changed = put("runs", (await read("/api/runs")).runs || []) || changed;
  const going = state.runs.find((r) => r.state === "running");
  changed = put("current", going ? await read("/api/status?run=" + going.id) : null) || changed;
  changed = put("schedules", await read("/api/schedules")) || changed;
  const key = runsKey();
  const need = state.route.name;
  // Findings change when a run ends or something is sent or marked.
  if (key !== state.findingsFor) {
    state.findings = (await read("/api/findings")).findings || [];
    state.findingsFor = key;
    changed = true;
  }
  // The grid is read when a run ends, and now and then while one goes.
  const staleGrid = !state.grid || state.gridKey !== key || (going && Date.now() - state.gridAt > 5000) || Date.now() - state.gridAt > 60000;
  if (staleGrid) changed = put("grid", await read("/api/grid")) || changed;
  if (staleGrid) {
    state.gridKey = key;
    state.gridAt = Date.now();
  }
  const staleFleet = !state.fleet || (going && Date.now() - state.fleetAt > 3000) || Date.now() - state.fleetAt > 30000;
  if ((["home", "fleet", "phone"].includes(need) || !state.fleet) && staleFleet) {
    changed = put("fleet", await read("/api/fleet")) || changed;
    state.fleetAt = Date.now();
  }
  if (need === "run") {
    const before = JSON.stringify(state.runView);
    await loadRun(state.route.args[0]);
    changed = changed || before !== JSON.stringify(state.runView);
  }
  if (need === "failure" || need === "test") {
    const before = JSON.stringify([state.failure, [...state.details.keys()]]);
    await loadRoute();
    changed = changed || before !== JSON.stringify([state.failure, [...state.details.keys()]]);
  }
  if (!state.page) state.page = await read("/api/page");
  if (changed) render();
}

/** Follows the page: often while a run goes, now and then otherwise. */
async function poll() {
  try {
    await refresh(false);
  } catch (err) {
    // A failed read shows on the next one; keep following.
  }
  setTimeout(poll, state.current ? 1500 : 10000);
}

async function start() {
  state.page = await read("/api/page");
  const list = await read("/api/list");
  state.tests = list.tests || [];
  state.categories = list.categories || [];
  state.presets = list.presets || [];
  state.machines = (await read("/api/machines")).machines || [];
  // QEMU is picked to start with: it is always there.
  if (state.machines.some((m) => m.name === "qemu")) state.picked.add("qemu");
  state.runs = (await read("/api/runs")).runs || [];
  await refresh(true);
  window.addEventListener("hashchange", route);
  await route();
  poll();
}

start();
