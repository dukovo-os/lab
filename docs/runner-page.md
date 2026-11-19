<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# The runner page

The runner page is the lab's control panel: pick machines and tests, run
them, watch the run, read what failed and send it on. It runs on the lab
PC and only a browser on that PC can open it; a phone can see a read-only
view of it ([Sharing read only](#sharing-read-only)).

```sh
./ob lab ui              # in a Dukovo checkout: builds the lab image, then serves the page
dukovo-lab ui --image <file> --build <text>   # the same without ob
```

It prints an address such as `http://127.0.0.1:7420/?key=…` and opens it
when there is a desktop. The key is new each time the page starts, so
an old address stops working; open the one printed last. Ctrl+C closes
the page. Runs started from it carry on, and the page shows them again
when it is opened next.

The page runs the image it was opened with, under that build's name. If
the image is rebuilt while the page is open, the page will not start
runs until it is opened again (`./ob lab ui`), so no run names the
wrong build.

## What is on it

One view shows at a time, chosen by the address's hash, so a view can be
shared and Back works: `#home`, `#tests`, `#test/<id>`,
`#failure/<run>/<machine>/<test>`, `#findings`, `#fleet`, `#runs`,
`#run/<id>`, `#start` and `#phone`. The top bar names the lab, links the views, and
shows the run going now with its progress and the time left, next to
**Start run**.

Every result is drawn with a glyph as well as a colour: ✓ passed (green),
✕ failed (red-orange), ~ flaky (amber), ◷ stalled, ! a lab fault or a
benched machine, – skipped (grey), ▸ running (blue).

- **Home.** Four tiles: tests failing now (new, known, flaky), lab faults
  this round (with the benched machine and why), coverage lost to skips by
  cause, and the round's time with how a pool split it. Under them, the
  tests-by-machines grid: a row for each test, a column for each machine
  that is in no pool and for each pool, the newest result in each cell, a
  class chip, and the last 14 runs. Filters Failing, New, Flaky and All,
  and a find box. **Needs a look** lists new Dukovo failures and lab
  faults, newest first; **Running now** each busy machine's test and
  progress, and the lab PC's load.
- **Failure.** One failed or broken result. Its class and why; Rerun (a run
  of that test on its machine, or its pool), Change class, Send to
  lab-results and Not a bug (with its reason, typed in place) or Reopen.
  Beside them: the same test on other machines and its rerun, the last 14
  runs and when it first went red, the run's conditions (health check,
  clock, root space, lab PC load, build, duration against the usual), the
  failed steps, the console's last 200 lines with links to the logs, and
  notes. The lab keeps no notes store, so Notes shows only the reason given
  with Not a bug.
- **Tests.** Per machine and pool, how many tests run alone, need a person
  or cannot run. The table filters (All, Run alone, Needs a person, Can't run
  anywhere real), by category and by find, with a chip for each machine. A
  test's panel gives what it checks, its limit and last time, the controls it
  uses, its file, a box listing exactly which steps a person does on each
  machine that needs one, its steps, **Run on** a machine or pool, and
  **History**.
- **Findings.** Every finding with its class, state, how often and where it
  failed, and whether it was sent. **Send all new** sends those not yet
  sent. Send needs a lab-results clone ([Sending findings](sending.md));
  `ob lab` makes it.
- **Fleet.** Each pool with a card for each machine: running, idle, benched
  or spare, what it is doing, the last health check as a list, and what
  its driver does and a person does. Machines in no pool follow. The
  schedules are drawn over 24 hours in Mountain time (America/Denver
  unless `schedules.json` names another zone), then listed with **New
  schedule**, **Edit** (Run now, Pause, Resume, Remove pressed twice) and
  their findings: Send when done or Hold for me. **Waiting for a person**
  lists the steps that going runs wait on, each with a **Done** button.
  Hardware changes between boots are not shown: the lab does not record
  them.
- **Past runs** and **Run.** Every run, newest first, with its findings
  and whether they were sent. A run's page has its progress, per-machine
  counts, Stop, Done for a step waiting for a person, what failed with
  **Send** for its findings, what it skipped and a live log.
- **Start run.** Tick machines and pools (a run on a pool shares its tests
  among its machines), then tests, a whole category, or a preset, which
  shows how long it took last time. Tests that need a person on a picked
  machine are kept apart and run only when ticked; when one of their steps
  comes up, the run says what to do on which machine, with a Done button.

- **Phone** (`#phone`). The page for a screen 390 px wide: the round going
  now with its progress and the time left, the passed, failing and left
  counts, **New since last round** (new Dukovo failures and lab faults, each
  linking to its Failure view), each machine as running, idle, benched with
  why, or spare, and a line saying whether the push notice is set up. With no
  round going it shows the last one's counts. On a narrow screen Home
  offers a link to it.

## Sharing read only

```sh
dukovo-lab ui --share 100.101.102.103 --image <file> --build <text>
```

`--share` takes one address of the lab PC and also listens on it, at the
same port, for a phone. It prints a second address with a key of its own:

```
The phone view is at http://100.101.102.103:7420/?key=…#phone
```

Open it once on the phone: the key is traded for a cookie, and the page
opens on `#phone`. The share key is made once and kept in `share_key` in
the configuration folder (readable by its owner alone, like the lab key),
so the address, and the `link` of a push notice, stay the same when `ui`
restarts. If the address leaks, give `--new-share-key` once, or delete
the file: the next start makes a new key, and every old address stops
working. The full page's own key is still new each start.

**Use the lab PC's Tailscale address** (`tailscale ip -4`). Tailscale's
network holds only your own devices, so only they can reach the page.
**Do not share a LAN or public address**: the key is the only thing between
whoever can reach the address and the lab's status, results and logs, and
it travels in the clear over plain HTTP, which Tailscale's encrypted
tunnel makes safe and a LAN or the internet does not. `--share` refuses
`0.0.0.0` and `::`, which mean every network, and warns about any address
outside Tailscale's ranges.

The read-only address:

- serves only GET requests, the ones the views read, and answers every
  other method, so every action (run, stop, done, send, mark, schedule),
  with 403, even with its key. The page is told it is read only and shows
  no action buttons;
- answers only to its own address, needs its own key, and does not take
  the full page's key; the full page does not take the phone's;
- keeps the page's security headers, so nothing loads from elsewhere.

It shows everything the page reads, results bundle logs and screenshots
included. Starting runs, sending findings and every other action stay on
the lab PC's own page.

## On your phone

The lab can push a notice when a round ends ([Notices](schedules.md#notices)).

1. Install the ntfy app (Android: Google Play or F-Droid; iPhone: the App
   Store).
2. Pick a topic name that cannot be guessed: **the topic name is the only
   secret.** Anyone who knows it can read the lab's notices and send
   others to it. Make it long and random, such as
   `dukovo-lab-7q4k9x2m5v8c1w3z` (`head -c 16 /dev/urandom | base32`), not
   `dukovo-lab`.
3. Subscribe to it in the app. For ntfy.sh, nothing else is needed.
4. On the lab PC, write the topic into `notify.toml`
   (`ntfy = "https://ntfy.sh/<topic>"`), and `chmod 600` the file.
5. Run `dukovo-lab notify --test`. The notice should arrive in a moment.
   To open the phone view from it, add `link` (see Notices).

The notice carries only counts and the names of tests and machines. A
server of your own (ntfy runs anywhere) keeps even those off ntfy.sh.

## Classes

Each failed or broken result has a class, which says whose it is:

| Class | Meaning |
| --- | --- |
| Lab | The harness or the machine broke (an error, a failed health check). It says nothing about Dukovo and is never sent. |
| Dukovo | A test failed. The default for a failure. |
| Flaky | It failed, then passed when run again on the same build. |
| Known | Its finding has a fix proposed, approved or landed, or links to the issue. |
| Test | A person said the test is wrong, with Not a bug. |

**Change class** gives a failure's finding a class of your own, which holds
in every run it fails in; Worked out by the lab takes it back. Not a bug
and Reopen clear it. A lab fault has no finding, so it cannot be changed.

## Reruns

A run runs a test that failed once more before it ends: on another machine
of its pool, a spare first, else on the same machine afresh. The second
try is recorded beside the first (`rerun` in results.json), which stays the
result: counts and findings read only it. A failure that then passes is
flaky. `run --no-rerun` turns this off; schedules rerun.

## The same as the commands

Every button runs a `dukovo-lab` command, and the bottom of the page
shows the last one, such as `dukovo-lab run --detach network/link-pull
--machine qemu --image … --json`. So anything done on the page can be
done in a terminal or by an agent, and the other way round: a run
started with `dukovo-lab run` shows on the page too.

| On the page | Command |
| --- | --- |
| Run | `run --detach <tests> --machine <a,b> [--with-person]` |
| A preset's Run | `run --detach --preset <name> --machine <a,b>` |
| This run | `status <run>` |
| Stop | `stop <run>` |
| Done | `done <run> --machine <name>` |
| Home, Tests grid | `grid` |
| Fleet | `fleet`, `schedule` |
| A test's panel | `show <test>` |
| Findings | `results <run>`, `findings --all` |
| Send this finding | `send <finding>` |
| Send, Send all new | `send --run <run>`, `send --all` |
| Not a bug, Reopen | `findings <finding> --not-a-bug [--note <text>]`, `findings <finding> --reopen` |
| Change class | `findings <finding> --class <class>` |
| Schedules | `schedule` |
| Make schedule, Save | `schedule save <name> ...` |
| Run now, Pause, Resume, Remove | `schedule start`, `pause`, `resume`, `remove <name>` |
| Past runs | `runs` |
| Tests, Machines | `list`, `machines` |

## Safety

The page listens on 127.0.0.1 only (and, read only, on the one address
`--share` names), answers only to that address and
`localhost`, needs the printed key (which it keeps in a cookie), and
takes actions only from its own script. It serves its own files, the
commands' answers and files inside results bundles; never anything else
on the PC, and never the lab key. Send pushes only bundles and findings,
as `dukovo-lab send` does.

The lab PC is meant to have one account. If it has others, know that
opening the browser hands it the address, key and all, and other
accounts can see a program's arguments while it runs; give `--no-open`
and paste the printed address into the browser yourself.
