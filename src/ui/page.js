// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The runner page's script. Everything it shows comes from a dukovo-lab
// command the server runs for it (see ui.rs), and everything it does is
// one of those commands: the last one run is shown at the bottom of the
// page, so a person can always see what to type to do the same.
//
// Text from tests, machines and logs is only ever set as text, never as
// HTML.

"use strict";

/** What the page knows; render() draws it all. */
const state = {
  page: null, // { version, build, image, can_run, can_send, results }
  tests: [], // from `list --json`
  categories: [],
  presets: [],
  machines: [], // from `machines --json`
  runs: [], // from `runs --json`, newest first
  picked: new Set(), // machine names
  chosen: new Set(), // test ids
  preset: "", // the preset the chosen tests came from, if they still match it
  find: "",
  opened: new Set(), // test ids whose details are shown
  viewing: null, // the run shown under "This run"
  status: null, // `status --json` of that run
  results: null, // `results --json` of that run
  resultsFor: "", // which status the results were fetched for
  findings: [], // from `findings --all --json`, most recently seen first
  findingsFor: "", // which runs the findings were fetched for
  open: new Set(), // findings panel items opened, by run:machine:test
  tabs: new Map(), // and the view shown in each
  noting: new Map(), // finding id to the Not a bug note being typed
  sending: false, // a send is going
  drawn: "", // what the findings panel was last drawn from
  schedules: null, // from `schedule --json`
  editing: null, // the schedule form's values while it is open
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

/** A new element: el("span", { class: "pill" }, "text", child...). */
function el(tag, attributes, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes || {})) {
    if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
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

/** "45 s", "12 min", "3 h 40 min". */
function duration(seconds) {
  if (seconds < 60) return Math.max(0, Math.round(seconds)) + " s";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return minutes + " min";
  const hours = Math.floor(minutes / 60);
  return hours + " h" + (minutes % 60 ? " " + (minutes % 60) + " min" : "");
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

const WORDS = { pass: "Passed", fail: "Failed", skip: "Skipped", error: "Error", running: "Running", queued: "Queued" };

function pill(kind, text) {
  return el("span", { class: "pill " + kind }, text || WORDS[kind] || kind);
}

/** How a test fits a machine: "runs", "needs-person" or "lacks". */
function fit(test, machine) {
  const entry = test.machines.find((m) => m.machine === machine);
  return entry ? entry.fit : "runs";
}

/** Tests that need a person on a picked machine: the Needs you group. */
function needsPerson(test) {
  return [...state.picked].some((machine) => fit(test, machine) === "needs-person");
}

/** About how long tests take on the picked machines, from earlier runs. */
function typicalSeconds(ids) {
  let longest = 0;
  for (const machine of state.picked) {
    let total = 0;
    for (const id of ids) {
      const test = state.tests.find((t) => t.id === id);
      const last = test && test.last_milliseconds[machine];
      if (last === undefined) return null;
      total += last;
    }
    longest = Math.max(longest, total);
  }
  return longest / 1000;
}

/** Where a test is in the run being viewed, across its machines. */
function testState(id) {
  if (!state.status) return null;
  const entries = state.status.tests.filter((t) => t.test === id);
  if (!entries.length) return null;
  const kinds = entries.map((t) => t.state);
  for (const kind of ["running", "error", "fail", "queued", "pass"]) {
    if (kinds.includes(kind)) return kind;
  }
  return "skip";
}

// ---- Drawing ----

function render() {
  renderHeader();
  renderMachines();
  renderTests();
  renderRun();
  renderFindings();
  renderSchedules();
  renderRuns();
}

function renderHeader() {
  const page = state.page || {};
  byId("build").textContent = page.build || (page.image ? "unnamed build" : "no image");
  byId("picked-count").textContent =
    state.picked.size + " of " + state.machines.length + " machines picked";
  if (page.can_run === false) {
    notice("This page was opened without an image, so it can show runs but not start them. Open it with ob lab ui.");
  }
}

function renderMachines() {
  const busy = new Map(); // machine name to the run using it
  for (const run of state.runs) {
    if (run.state === "running") run.machines.forEach((m) => busy.set(m, run));
  }
  const cards = state.machines.map((machine) => {
    const on = state.picked.has(machine.name);
    const run = busy.get(machine.name);
    const status = run
      ? pill("running", "Running · " + duration((Date.now() - new Date(run.started)) / 1000))
      : pill("ready", "Ready");
    const byPerson = Object.entries(machine.controls)
      .filter(([, who]) => who === "person")
      .map(([control]) => control);
    const about = machine.about + (byPerson.length ? " · you do: " + byPerson.join(", ") : "");
    const box = el("input", {
      type: "checkbox",
      checked: on,
      "aria-label": "Run on " + machine.name,
      onchange: () => {
        on ? state.picked.delete(machine.name) : state.picked.add(machine.name);
        render();
      },
    });
    return el("label", { class: "mc" + (on ? " on" : "") }, box, el("span", { class: "mn" }, machine.name), status, el("span", { class: "ms" }, about));
  });
  byId("machines").replaceChildren(...cards);
}

function renderTests() {
  const find = state.find.toLowerCase();
  const shown = state.tests.filter(
    (t) => !find || t.id.includes(find) || t.about.toLowerCase().includes(find),
  );
  const needs = shown.filter(needsPerson);
  const groups = state.categories.map((category) => ({
    title: category.title,
    tests: shown.filter((t) => t.category === category.id && !needsPerson(t)),
    all: state.tests.filter((t) => t.category === category.id),
  }));
  const nodes = groups.map((group) => category(group.title, group.tests, group.all, false));
  if (needs.length) nodes.push(category("Needs you", needs, needs, true));
  byId("categories").replaceChildren(...nodes);

  const chosen = state.tests.filter((t) => state.chosen.has(t.id));
  byId("test-count").textContent =
    "· " + state.tests.length + " tests · " + chosen.length + " picked";
  // The preset list, with how long each took last time on these machines.
  const select = byId("preset");
  const options = [el("option", { value: "" }, "Picked by hand")];
  for (const preset of state.presets) {
    const seconds = typicalSeconds(preset.tests);
    const label = preset.title + (seconds != null ? " (about " + duration(seconds) + ")" : "");
    options.push(el("option", { value: preset.name, selected: preset.name === state.preset }, label));
  }
  select.replaceChildren(...options);
  const button = byId("run-picked");
  const preset = state.presets.find((p) => p.name === state.preset);
  button.textContent = preset ? "Run " + preset.title : "Run " + chosen.length + " picked";
  button.disabled = !state.page || !state.page.can_run || !chosen.length || !state.picked.size;
}

/** One category of tests, or the Needs you group. */
function category(title, tests, all, needs) {
  const ids = all.map((t) => t.id);
  const picked = ids.filter((id) => state.chosen.has(id)).length;
  const box = el("input", {
    type: "checkbox",
    checked: ids.length > 0 && picked === ids.length,
    disabled: !ids.length,
    "aria-label": "All " + title + " tests",
    // Inside a summary a click would also open or close the group.
    onclick: (event) => {
      event.preventDefault();
      const every = picked === ids.length;
      ids.forEach((id) => (every ? state.chosen.delete(id) : state.chosen.add(id)));
      state.preset = "";
      render();
    },
  });
  const runAll = el("button", {
    class: "mini",
    type: "button",
    disabled: !ids.length || !state.page || !state.page.can_run || !state.picked.size,
    onclick: (event) => {
      event.preventDefault();
      startRun(ids, null);
    },
  }, "Run all " + ids.length);
  const count = ids.length ? picked + " of " + ids.length : "no tests yet";
  const summary = el("summary", {}, box, el("span", { class: "name" }, title), el("span", { class: "count" }, count), runAll);
  const list = el("ul", { class: "tests" }, tests.flatMap(testRows));
  const group = el("details", { class: "cat" + (needs ? " needs" : ""), open: tests.length > 0 }, summary, list);
  if (needs) {
    group.append(el("p", { class: "note pad" }, "These ask you to do a step, such as pulling a cable, and wait until you press Done. They run only when ticked."));
  }
  return group;
}

/** A test's row, and its details when opened. */
function testRows(test) {
  const chosen = state.chosen.has(test.id);
  const box = el("input", {
    type: "checkbox",
    checked: chosen,
    "aria-label": "Pick " + test.id,
    onchange: () => {
      chosen ? state.chosen.delete(test.id) : state.chosen.add(test.id);
      state.preset = "";
      render();
    },
  });
  const name = el("button", {
    class: "tname",
    type: "button",
    "aria-expanded": state.opened.has(test.id) ? "true" : "false",
    onclick: () => {
      state.opened.has(test.id) ? state.opened.delete(test.id) : state.opened.add(test.id);
      render();
    },
  }, test.id);
  const kind = testState(test.id);
  const run = el("button", {
    class: "mini",
    type: "button",
    disabled: !state.page || !state.page.can_run || !state.picked.size,
    onclick: () => startRun([test.id], null),
  }, "Run");
  const rows = [el("li", {}, box, el("label", {}, name, el("span", { class: "why" }, test.about)), kind ? pill(kind) : el("span"), run)];
  if (state.opened.has(test.id)) rows.push(testDetail(test));
  return rows;
}

function testDetail(test) {
  const fits = test.machines.map((m) => {
    if (m.fit === "runs") return m.machine;
    if (m.fit === "needs-person") return m.machine + " (you do " + m.controls.join(", ") + ")";
    return m.machine + " (cannot: no " + m.controls.join(", ") + ")";
  });
  const last = Object.entries(test.last_milliseconds).map(([m, ms]) => m + " " + duration(ms / 1000));
  return el("li", { class: "tdetail" }, el("div", { class: "td" },
    el("div", {}, el("strong", {}, "What " + test.id + " does"),
      el("div", { class: "note" }, test.file + " · time limit " + test.time + (test.boot === "stick" ? " · boots the install stick" : test.fresh ? " · boots afresh" : ""))),
    el("ol", { class: "steps" }, el("li", {}, "Boot the image, or share the boot the test before left."), test.steps.map((s) => el("li", {}, s.step))),
    el("div", { class: "note" }, "Passes when every step does. Machines: " + fits.join(", ") + "." + (last.length ? " Last time: " + last.join(", ") + "." : "")),
  ));
}

function renderRun() {
  const status = state.status;
  byId("no-run").hidden = !!status;
  byId("progress").hidden = !status;
  byId("log").hidden = !status;
  const stop = byId("stop");
  stop.hidden = !status || status.state !== "running";
  if (!status) {
    byId("waiting").replaceChildren();
    return;
  }
  const newest = state.runs.length && state.runs[0].id === status.id;
  const word = { running: "", ended: "", stopped: " (stopped)", died: " (its process ended)" }[status.state];
  const title = status.state === "running" ? "This run" : newest ? "Last run" : "Run";
  byId("run-title").textContent = title + " · " + when(status.started) + word;

  // Steps waiting for a person.
  byId("waiting").replaceChildren(...status.waiting.map((wait) =>
    el("div", { class: "wait" },
      el("span", {}, el("strong", {}, "Needs you on " + wait.machine + ": "), wait.step + " (" + wait.test + "), then press Done."),
      el("button", { class: "btn primary", type: "button", onclick: () => act("/api/done", { run: status.id, machine: wait.machine }).then(refresh) }, "Done"))));

  const c = status.counts;
  const total = c.passed + c.failed + c.skipped + c.errors + c.running + c.queued || 1;
  const bar = (cls, n) => {
    const part = el("i", { class: cls });
    part.style.width = (100 * n) / total + "%";
    return part;
  };
  const legend = [c.passed + " passed", c.failed + " failed"];
  if (c.errors) legend.push(c.errors + " errors");
  if (c.skipped) legend.push(c.skipped + " skipped");
  if (status.state === "running") {
    legend.push(c.running + " running", c.queued + " queued");
    if (status.left_seconds != null) legend.push("about " + duration(status.left_seconds) + " left");
  }
  const machines = status.machines.map((m) =>
    el("span", {}, el("strong", {}, m.name), " " + m.counts.passed + " passed, " + (m.counts.failed + m.counts.errors) + " failed" + (m.running ? ", running " + m.running : "")));
  const track = el("div", { class: "track", role: "img", "aria-label": legend.join(", ") },
    bar("t-pass", c.passed), bar("t-fail", c.failed + c.errors), bar("t-skip", c.skipped), bar("t-run", c.running));
  byId("progress").replaceChildren(track, el("div", { class: "legend" }, legend.map((t) => el("span", {}, t))), el("div", { class: "legend" }, machines));
  if (status.output) byId("progress").append(el("pre", { class: "log" }, status.output));

  const log = byId("log");
  const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
  log.replaceChildren(...status.recent.map(logLine).filter(Boolean));
  if (atBottom) log.scrollTop = log.scrollHeight;
}

/** One event as a line of the live log. */
function logLine(event) {
  const time = event.at ? new Date(event.at).toLocaleTimeString([], { hour12: false }) : "";
  const where = event.machine ? event.machine + " · " + event.test : "run";
  let text;
  let bad = false;
  switch (event.event) {
    case "run-started": text = "started " + event.tests.length + " tests on " + event.machines.join(", "); break;
    case "test-started": text = "started"; break;
    case "step-ended":
      bad = event.result === "fail" || event.result === "error";
      text = event.step + ": " + (event.result === "pass" ? "ok" : event.result.toUpperCase()) + " (" + duration(event.milliseconds / 1000) + ")";
      break;
    case "person-needed": text = "NEEDS YOU: " + event.step; break;
    case "test-ended":
      bad = event.result === "fail" || event.result === "error";
      text = WORDS[event.result].toUpperCase() + (event.reason ? ": " + event.reason.split("\n")[0] : "");
      break;
    case "run-ended": {
      const found = (event.findings || []).length;
      text = (event.stopped ? "stopped" : "ended") + (found ? " · " + found + (found === 1 ? " finding" : " findings") : "");
      break;
    }
    default: return null;
  }
  return el("div", {}, el("span", { class: "t" }, time), " ", el("span", { class: "k" }, where), " ", el("span", { class: bad ? "e" : "" }, text));
}

// ---- Findings ----
//
// A failed result belongs to a finding: the same test failing the same
// way, in every run it did (see findings.rs). A result that broke (an
// error) is a lab problem instead, which says nothing about Dukovo, so
// it is shown but never sent.

const STATE_WORDS = { new: "New", "not-a-bug": "Not a bug", "fix-proposed": "Fix proposed", approved: "Fix approved", fixed: "Fixed" };

/** Whether a finding failed in a run lab-results does not have yet. */
function unsent(finding) {
  return finding.runs.some((seen) => !seen.sent);
}

/** Whether Send has something of a finding's to send. */
function sendable(finding) {
  return finding.state !== "not-a-bug" && unsent(finding);
}

/** The finding a failed result belongs to, if it is known yet. */
function findingOf(run, result) {
  return state.findings.find((f) => f.test === result.test && f.runs.some((s) => s.run === run && s.machine === result.machine));
}

/** The findings of a run, each once. */
function findingsOfRun(run) {
  return state.findings.filter((f) => f.runs.some((s) => s.run === run));
}

function renderFindings() {
  const results = state.results;
  const broken = results ? results.results.filter((r) => r.result === "fail" || r.result === "error") : [];
  const run = results ? results.run.id : "";
  const ofRun = findingsOfRun(run);
  const allNew = state.findings.filter(sendable);
  // Drawn again only when what it shows changed: a redraw would close
  // what a person opened and lose a note being typed.
  const drawn = JSON.stringify([state.resultsFor, run, broken.length, ofRun, allNew.length, state.sending, state.page, [...state.noting.keys()]]);
  if (drawn === state.drawn) return;
  state.drawn = drawn;

  byId("findings-panel").hidden = !broken.length;
  if (!broken.length) return;
  const errors = broken.filter((r) => r.result === "error").length;
  const counts = [ofRun.length + (ofRun.length === 1 ? " finding" : " findings")];
  if (errors) counts.push(errors + (errors === 1 ? " lab problem" : " lab problems"));
  byId("finding-count").textContent = "· " + counts.join(", ");
  byId("findings").replaceChildren(...broken.map((result) => item(results, result)));

  // Send, for this run's findings; and for every finding, when others wait too.
  const canSend = state.page && state.page.can_send;
  const toSend = ofRun.filter(sendable);
  const bar = byId("send-bar");
  const button = byId("send-run");
  bar.hidden = !ofRun.length;
  if (!canSend) {
    byId("send-note").textContent = "Send needs the lab-results clone at " + ((state.page && state.page.results) || "lab-results in the store") + ": ob lab makes it, or see docs/sending.md.";
  } else if (toSend.length) {
    byId("send-note").textContent = "Sends the findings, their runs and logs to the project's private lab-results repository. A thread proposes a fix for each, and nothing is fixed until you approve it.";
  } else {
    byId("send-note").textContent = "Everything here is in lab-results, or marked not a bug.";
  }
  button.hidden = !toSend.length;
  button.disabled = !canSend || state.sending;
  button.textContent = state.sending ? "Sending…" : "Send " + toSend.length + (toSend.length === 1 ? " finding" : " findings");
  const all = byId("send-all");
  all.hidden = allNew.length <= toSend.length;
  all.disabled = !canSend || state.sending;
  all.textContent = "Send all new (" + allNew.length + ")";
}

/** One broken result: its finding (or lab problem), why, its logs, and what to do. */
function item(results, result) {
  const run = results.run.id;
  const finding = result.result === "fail" ? findingOf(run, result) : null;
  const key = run + ":" + result.machine + ":" + result.test;
  const file = (path) => "/files/" + run + "/" + path;
  const views = {
    Steps: () => el("ol", { class: "steps" }, result.steps.map((s) =>
      el("li", { class: s.result === "pass" ? "" : "bad" },
        s.step + ": " + (s.result === "pass" ? "ok" : s.result.toUpperCase()) + (s.note ? "\n" + s.note : "")))),
    "Test log": () => textFile(result.log && file(result.log)),
    "Serial log": () => textFile(result.boot && file("logs/" + result.machine + "/serial-" + result.boot + ".log")),
  };
  if (result.screenshots.length) {
    views["Screenshots (" + result.screenshots.length + ")"] = () =>
      el("div", { class: "shots" }, result.screenshots.map((shot) =>
        el("figure", {}, el("img", { src: file(shot), alt: "Screenshot " + shot }), el("figcaption", {}, shot.split("/").pop()))));
  }
  const tabs = el("div", { class: "tabs", role: "tablist" });
  const body = el("div");
  const show = (name) => {
    state.tabs.set(key, name);
    for (const tab of tabs.children) tab.classList.toggle("sel", tab.textContent === name);
    body.replaceChildren(views[name]());
  };
  for (const name of Object.keys(views)) {
    tabs.append(el("button", { class: "tab", type: "button", role: "tab", onclick: () => show(name) }, name));
  }

  const lines = [result.machine];
  let badge;
  if (finding) {
    badge = pill(finding.state, STATE_WORDS[finding.state]);
    lines.push(seenText(finding));
    if (finding.state === "not-a-bug" && finding.note) lines.push("not a bug: " + finding.note);
  } else if (result.result === "error") {
    badge = pill("lab-problem", "Lab problem");
    lines.push("the harness or the machine broke, so this says nothing about Dukovo; it is not sent");
  } else {
    badge = pill("fail");
  }
  const meta = lines.join(" · ") + "\n" + (result.reason || "").split("\n").slice(0, 3).join("\n");

  const detail = el("div", { class: "detail" }, tabs, body, actions(result, finding));
  const box = el("details", { class: "finding" + (finding ? (finding.state === "new" ? "" : " closed") : result.result === "error" ? " lab" : ""), open: state.open.has(key) },
    el("summary", {}, el("div", { class: "top" }, el("span", { class: "title" }, result.test), badge), el("div", { class: "meta" }, meta)),
    detail);
  // Logs are fetched only once a view shows: they can be long.
  const draw = () => box.open && !body.childElementCount && show(state.tabs.get(key) || "Steps");
  box.addEventListener("toggle", () => {
    box.open ? state.open.add(key) : state.open.delete(key);
    draw();
  });
  draw();
  return box;
}

/** "first time", or "failed 3 times on qemu, optiplex since Nov 3 19:05; passes again on qemu". */
function seenText(finding) {
  let text = finding.count === 1
    ? "first time"
    : "failed " + finding.count + " times on " + finding.machines.join(", ") + " since " + when(finding.first_seen);
  const passing = Object.keys(finding.passing || {});
  if (passing.length) text += "; passed since on " + passing.join(", ");
  return text;
}

/** Send, Run again, Not a bug (with a note typed in place) or Reopen. */
function actions(result, finding) {
  const canSend = state.page && state.page.can_send;
  const again = el("button", { class: "btn", type: "button", disabled: !state.page || !state.page.can_run,
    onclick: () => startRun([result.test], [result.machine]) }, "Run again");
  if (!finding) return el("div", { class: "tools" }, again);
  if (state.noting.has(finding.id)) {
    const input = el("input", { type: "text", maxlength: "500", "aria-label": "Why it is not a bug",
      placeholder: "Why, if you like: the test is wrong, the cable was out…",
      value: state.noting.get(finding.id),
      oninput: (event) => state.noting.set(finding.id, event.target.value),
      onkeydown: (event) => event.key === "Enter" && say(),
    });
    const say = () => changeFinding({ finding: finding.id, not_a_bug: true, note: input.value });
    const cancel = () => {
      state.noting.delete(finding.id);
      renderFindings();
    };
    setTimeout(() => input.focus(), 0);
    return el("div", { class: "nab" }, input,
      el("button", { class: "btn primary", type: "button", onclick: say }, "Say not a bug"),
      el("button", { class: "btn", type: "button", onclick: cancel }, "Cancel"));
  }
  const tools = el("div", { class: "tools" });
  if (finding.state !== "not-a-bug") {
    tools.append(el("button", {
      class: "btn primary", type: "button",
      disabled: !canSend || state.sending || !unsent(finding),
      title: canSend ? null : "Send needs a lab-results clone: ob lab makes it",
      onclick: () => sendNow({ findings: [finding.id] }),
    }, unsent(finding) ? (state.sending ? "Sending…" : "Send this finding") : "Sent"));
  }
  tools.append(again);
  if (finding.state === "not-a-bug") {
    tools.append(el("button", { class: "btn", type: "button", onclick: () => changeFinding({ finding: finding.id, reopen: true }) }, "Reopen"));
  } else {
    tools.append(el("button", { class: "btn", type: "button", onclick: () => {
      state.noting.set(finding.id, "");
      renderFindings();
    } }, "Not a bug"));
  }
  return tools;
}

/** A log file, fetched when shown. */
function textFile(url) {
  const pre = el("pre", { class: "log" }, url ? "Loading…" : "No log.");
  if (url) {
    fetch(url, { credentials: "same-origin" })
      .then((r) => (r.ok ? r.text() : "Not found."))
      .then((text) => (pre.textContent = text));
  }
  return pre;
}

function renderRuns() {
  byId("no-runs").hidden = state.runs.length > 0;
  byId("runs").replaceChildren(...state.runs.map((run) => {
    const s = run.summary;
    let result;
    if (run.state === "running") result = pill("running");
    else if (run.state === "died") result = pill("error", "Died");
    else if (run.state === "stopped") result = pill("skip", "Stopped · " + s.passed + " passed");
    else result = pill(s.failed || s.errors ? "fail" : "pass", s.passed + " of " + (s.passed + s.failed + s.errors));
    const preset = state.presets.find((p) => p.name === run.preset);
    let name = preset ? preset.title : run.tests + (run.tests === 1 ? " test" : " tests");
    if (run.schedule) name = scheduleTitle(run.schedule) + " (scheduled)";
    return el("tr", { class: run.id === state.viewing ? "sel" : "", tabindex: "0", onclick: () => view(run.id), onkeydown: (e) => e.key === "Enter" && view(run.id) },
      el("td", {}, when(run.started)), el("td", {}, name), el("td", {}, run.machines.join(", ")),
      el("td", { class: "mono" }, run.build || ""), el("td", { class: "res" }, result), el("td", {}, runFindings(run)));
  }));
}

/** A past run's findings, and whether they went to lab-results. */
function runFindings(run) {
  if (run.state === "running") return "";
  const found = findingsOfRun(run.id);
  if (!found.length) return run.sent ? "Sent" : "Nothing to send";
  let text = found.length + (found.length === 1 ? " finding" : " findings");
  if (found.every((f) => f.state === "fixed")) text += ", fixed";
  else if (found.every((f) => f.state === "not-a-bug")) text += ", not bugs";
  else text += run.sent ? ", sent" : found.some(sendable) ? ", not sent" : "";
  return text;
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

function renderSchedules() {
  const listed = state.schedules;
  if (!listed) return;
  const schedules = listed.schedules || [];
  byId("when-head").textContent = "When (" + (listed.zone_short || listed.zone) + ")";
  byId("schedules-table").hidden = !schedules.length;
  byId("no-schedules").hidden = schedules.length > 0 || !!state.editing;
  byId("schedules").replaceChildren(...schedules.map((schedule) => {
    let next;
    if (schedule.going) next = pill("running", schedule.going.run ? "Running" : "Starting");
    else if (schedule.paused) next = "Paused";
    else if (schedule.each_build) next = "On the next build on main";
    else next = schedule.next_text || "";
    const title = el("td", {}, scheduleTitle(schedule.name));
    const line = schedule.going ? schedule.going.doing : lastText(schedule.last);
    if (line) title.append(el("span", { class: "why" }, line));
    const edit = el("button", { class: "mini", type: "button", onclick: () => openForm(schedule) }, "Edit");
    return el("tr", {}, title, el("td", {}, schedule.when), el("td", {}, schedule.machines.join(", ")),
      el("td", {}, schedule.send ? "Send when done" : "Hold for me"), el("td", {}, next), el("td", {}, edit));
  }));
  let note = "Scheduled runs always skip tests that need you. A scheduled run waits if a run is already going on that machine, and tests main as it is then, on the lab's own Dukovo checkout.";
  if (listed.zone_problem) note += " Times cannot be worked out: " + listed.zone_problem;
  if (!listed.timer) note += " The timer that starts schedules is not installed yet: ob lab installs it.";
  byId("schedule-note").textContent = note;
}

/** How a schedule's last start went, in a few words. */
function lastText(last) {
  if (!last) return "";
  const words = { passed: "passed", failed: "failed", broke: "the lab broke", stopped: "stopped", missed: "missed", error: "could not run" };
  let text = "Last: " + when(last.started) + ", " + (words[last.result] || last.result);
  const count = (last.findings || []).length;
  if (count) text += ", " + count + (count === 1 ? " finding" : " findings") + (last.sent ? " sent" : " held");
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
  drawForm();
  renderSchedules();
  byId("schedule-form").scrollIntoView({ block: "nearest" });
}

function closeForm() {
  state.editing = null;
  byId("schedule-form").hidden = true;
  byId("schedule-form").replaceChildren();
  renderSchedules();
}

/** Draws the form from state.editing. Polls never redraw it, so what is typed stays. */
function drawForm() {
  const f = state.editing;
  const form = byId("schedule-form");
  const row = (label, ...content) => el("div", { class: "row" }, el("span", {}, label), el("div", { class: "opts" }, ...content));
  const radio = (name, value, checked, label, onpick) =>
    el("label", {}, el("input", { type: "radio", name, value, checked, onchange: onpick }), label);
  const box = (checked, label, onflip) => el("label", {}, el("input", { type: "checkbox", checked, onchange: onflip }), label);

  const name = el("input", { type: "text", value: f.name, placeholder: "nightly", disabled: !f.isNew, "aria-label": "Schedule name",
    oninput: (e) => (f.name = e.target.value.trim()) });
  const title = el("input", { type: "text", value: f.title, placeholder: "Full run", maxlength: "80", "aria-label": "What to call it",
    oninput: (e) => (f.title = e.target.value) });
  const options = state.presets.map((p) => el("option", { value: "preset:" + p.name, selected: f.what === "preset:" + p.name }, p.title));
  options.push(el("option", { value: "picked", selected: f.what === "picked" }, "The tests picked above (" + state.chosen.size + ")"));
  options.push(el("option", { value: "all", selected: f.what === "all" }, "Every test"));
  if (f.what === "own") options.push(el("option", { value: "own", selected: true }, "Its own tests (" + f.tests.length + ")"));
  const what = el("select", { "aria-label": "What runs", onchange: (e) => (f.what = e.target.value) }, options);
  const machines = state.machines.map((m) => box(f.machines.has(m.name), m.name, () => {
    f.machines.has(m.name) ? f.machines.delete(m.name) : f.machines.add(m.name);
  }));
  const time = el("input", { type: "time", value: f.at, "aria-label": "Time", disabled: f.kind !== "time",
    oninput: (e) => (f.at = e.target.value) });
  const days = DAYS.map(([day, label]) => {
    const input = el("input", { type: "checkbox", checked: f.days.has(day), disabled: f.kind !== "time",
      onchange: () => (f.days.has(day) ? f.days.delete(day) : f.days.add(day)) });
    return el("label", {}, input, label);
  });
  const pick = (kind) => () => {
    f.kind = kind;
    drawForm();
  };
  const zone = (state.schedules && (state.schedules.zone_short || state.schedules.zone)) || "";

  const buttons = [el("button", { class: "btn primary", type: "submit" }, f.isNew ? "Make schedule" : "Save")];
  buttons.push(el("button", { class: "btn", type: "button", onclick: closeForm }, "Cancel"));
  const others = [];
  if (!f.isNew) {
    others.push(el("button", { class: "btn", type: "button", onclick: () => scheduleAction({ start: f.name }) }, "Run now"));
    others.push(el("button", { class: "btn", type: "button", onclick: () => scheduleAction(f.paused ? { resume: f.name } : { pause: f.name }) }, f.paused ? "Resume" : "Pause"));
    others.push(el("button", { class: "btn danger", type: "button", onclick: () => {
      if (!f.removing) {
        f.removing = true;
        drawForm();
        return;
      }
      scheduleAction({ remove: f.name });
    } }, f.removing ? "Remove it for good" : "Remove"));
  }

  form.replaceChildren(
    el("h3", {}, f.isNew ? "New schedule" : "Change " + f.name),
    row("Name", name),
    row("Call it", title),
    row("Runs", what),
    row("On", ...machines),
    row("When", radio("kind", "time", f.kind === "time", "At", pick("time")), time, el("span", { class: "note" }, zone)),
    row("", ...days),
    row("", radio("kind", "build", f.kind === "build", "On each new build on main", pick("build"))),
    row("Findings", radio("send", "send", f.send, "Send when done", () => (f.send = true)),
      radio("send", "hold", !f.send, "Hold for me", () => (f.send = false))),
    el("div", { class: "acts" }, el("div", {}, ...buttons), el("div", {}, ...others)),
  );
  form.onsubmit = (event) => {
    event.preventDefault();
    saveSchedule();
  };
  form.hidden = false;
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
  if (body.start) notice("Schedule " + body.start + " started. It shows under This run once its image is built.");
  await refresh();
}

// ---- Acting ----

/** Sends to lab-results, then shows what is sent. */
async function sendNow(body) {
  notice("");
  state.sending = true;
  renderFindings();
  try {
    await act("/api/send", body);
  } finally {
    state.sending = false;
    state.findingsFor = "";
    await refresh();
  }
}

/** Says a finding is not a bug, or reopens it. */
async function changeFinding(body) {
  notice("");
  const answer = await act("/api/findings", body);
  if (!answer.error) state.noting.delete(body.finding);
  state.findingsFor = "";
  await refresh();
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
    return test && needsPerson(test);
  });
  const answer = await act("/api/run", body);
  if (answer.id) {
    state.viewing = answer.id;
    await refresh();
  }
}

async function view(id) {
  state.viewing = id;
  state.results = null;
  state.resultsFor = "";
  await refreshRun();
  render();
}

/** Reads the viewed run's status, and its results when they changed. */
async function refreshRun() {
  if (!state.viewing) return;
  state.status = await read("/api/status?run=" + state.viewing);
  const c = state.status.counts;
  const key = state.status.id + ":" + state.status.state + ":" + (c.passed + c.failed + c.skipped + c.errors);
  if (key !== state.resultsFor) {
    state.results = await read("/api/results?run=" + state.viewing);
    state.resultsFor = key;
  }
}

async function refresh() {
  state.runs = (await read("/api/runs")).runs || [];
  state.schedules = await read("/api/schedules");
  // Findings change when a run ends or something is sent or marked.
  const findingsFor = state.runs.map((r) => r.id + r.state + r.sent).join();
  if (findingsFor !== state.findingsFor) {
    state.findings = (await read("/api/findings")).findings || [];
    state.findingsFor = findingsFor;
  }
  if (!state.viewing && state.runs.length) {
    const going = state.runs.find((r) => r.state === "running");
    state.viewing = (going || state.runs[0]).id;
  }
  await refreshRun();
  render();
}

/** Follows the page: often while a run goes, now and then otherwise. */
async function poll() {
  try {
    await refresh();
  } catch (err) {
    // A failed read shows on the next one; keep following.
  }
  const going = state.status && state.status.state === "running";
  setTimeout(poll, going ? 1500 : 10000);
}

async function start() {
  byId("find").addEventListener("input", (event) => {
    state.find = event.target.value;
    renderTests();
  });
  byId("preset").addEventListener("change", (event) => {
    state.preset = event.target.value;
    const preset = state.presets.find((p) => p.name === state.preset);
    if (preset) state.chosen = new Set(preset.tests);
    render();
  });
  byId("run-picked").addEventListener("click", () => startRun(null, null));
  byId("send-run").addEventListener("click", () => sendNow({ run: state.results.run.id }));
  byId("send-all").addEventListener("click", () => sendNow({ all: true }));
  byId("new-schedule").addEventListener("click", () => openForm(null));
  byId("stop").addEventListener("click", async () => {
    byId("stop").disabled = true;
    byId("stop").textContent = "Stopping…";
    await act("/api/stop", { run: state.status.id });
    byId("stop").disabled = false;
    byId("stop").textContent = "Stop";
    await refresh();
  });

  state.page = await read("/api/page");
  const list = await read("/api/list");
  state.tests = list.tests || [];
  state.categories = list.categories || [];
  state.presets = list.presets || [];
  state.machines = (await read("/api/machines")).machines || [];
  // QEMU is picked to start with: it is always there.
  if (state.machines.some((m) => m.name === "qemu")) state.picked.add("qemu");
  render();
  poll();
}

start();
