<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Commands

```
dukovo-lab [options] <command> [arguments]
```

In Dukovo's repository, `./ob lab <command>` runs the same commands with
the folders and image filled in: it builds the `--lab` image when it is
out of date and passes `--image`, `--tests lab/tests` and `--build`.

Every command takes `--json`, never asks a question, and exits with one
of the statuses below, so a script or an agent drives the lab as a
person does.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--tests <dir>` | `lab/tests` | The test folder. |
| `--config <dir>` | `~/.config/dukovo-lab` | Machine files (`machines/`), the schedules (`schedules.json`), the push notice's setting (`notify.toml`) and the lab key. |
| `--store <dir>` | `~/.local/share/dukovo-lab` | Results bundles (`runs/`) and the run's work files while it runs. |
| `--json` | | Answer in JSON. |

## list

```
dukovo-lab list [<test or category>...] [--machine <a,b>]
```

The tests by category, with their time limits, and a line for each
machine a test does not simply run on. With `--json`:

```json
{"tests": [{"id": "network/link-pull", "category": "network", "about": "...", "time": "3m",
  "fresh": false, "file": "lab/tests/network/link-pull.toml",
  "controls": ["power", "serial", "link", "ssh"],
  "machines": [{"machine": "qemu", "fit": "runs", "controls": []}],
  "steps": [{"step": "pull the network cable", "control": "link"}],
  "last_milliseconds": {"qemu": 61500}}],
 "categories": [{"id": "boot", "title": "Boot and power", "about": "..."}],
 "presets": [{"name": "quick", "title": "Quick check", "about": "...", "tests": ["boot/reaches-ready"]}]}
```

`fit` is `runs`, `needs-person` (with the controls a person does) or
`lacks` (with the controls the machine cannot do). `last_milliseconds`
is how long the test took on each machine the last time it ran there,
from the store's newest runs; the runner page adds them up for a time
estimate. `presets` come from `presets.toml` in the test folder (see
[test files](test-files.md#presets)).

## show

```
dukovo-lab show <test>
```

One test: what it checks, its file, its time limit, its controls, its
steps in words and how it fits each machine. With `--json`, the same
object as in `list`, and `host_needs` (programs the lab PC must have),
`pools` (how it fits each pool, named `pool:<name>`), `last_milliseconds`
and `history`: its newest results on every machine, each with run, time,
build, machine, result, `flaky`, class and the first line of its reason.

## grid

```
dukovo-lab grid
```

The newest result of each test on each machine that is in no pool and on
each pool, read from the newest 100 runs. With `--json`: `columns`, and
`rows` of `{test, category, about, failing, new, class, history, red_since,
cells}`. A cell holds `result`, `state` (`pass`, `fail`, `flaky`, `stalled`,
`error`, `benched`, `skip`), `class`, the first line of `reason`, `skip_group`,
`milliseconds`, `run`, `at`, `machine` (the one that ran it, for a pool),
`rerun`, `finding`, `history` (its last 14 results, oldest first: `p` passed,
`f` failed, `k` flaky, `e` error, `x` skipped), `streak` (red results in a
row) and `red_since` (the run, time and build it first went red in). `running`
lists tests going now, and `round` the newest run that ended and how its time
split.

## fleet

```
dukovo-lab fleet
```

Each machine with who does each control, its last health check (the line
from the newest run that booted it, as a list of checks), whether the newest
run it took part in benched it, and what it is doing now; the lab PC's
load; and the steps waiting for a person in runs going. With `--json`:
`{"lab_pc_load", "machines", "waiting"}`.

## machines

```
dukovo-lab machines
```

Each machine, its file and who does each control, and its pool if it has
one (`pool: optiplex`, with `a spare a run on the pool leaves out` for a
spare; see [Machine files](machine-files.md#pools)). With `--json`:
`{"machines": [{"name", "about", "file", "driver", "ready", "pool", "spare",
"controls": {"power": "driver", ...}, "qemu": {...}, "amt": {...},
"plug": {...}}]}`; `pool` and `spare` are left out when there is none.

## machines check

```
dukovo-lab machines check [<machine>...]
```

Checks what a run on each real machine needs (default: every machine),
without changing anything: AMT's password file, AMT itself (its version,
Admin Control Mode, the machine's power, whether it can ask the system
to shut down), the serial port, the lab PC's address on the lab network,
the net-boot server's port, the TFTP folder, the network GRUB, the
writer's kit and the lab key. For a plug machine: the plug (that it
answers, its switch and the machine's draw), the lab network, the
net-boot server's port, the TFTP folder, the network GRUB, the writer's
kit, the lab key and the netlog port. QEMU machines have nothing to check. Each
check is `ok`, a `note`, or a `problem`; the command exits 3 while any
check is a problem. With `--json`:
`{"machines": [{"name", "checks": [{"check", "verdict", "detail"}]}]}`.
See [Real machines](real-machines.md).

## run

```
dukovo-lab run <test or category>... | --all | --preset <name> --image <file>
               [--machine <a,b>] [--build <text>] [--linux <dir>] [--redteam <dir>]
               [--fonts <dir>] [--browsers <dir>] [--stick <file>] [--dvd <file>] [--ob <file>]
               [--with-person] [--no-rerun] [--notify] [--detach]
dukovo-lab run --resume <run> --image <file> [--build <text>] [--detach]
```

Runs the tests on each machine (default `qemu`), the machines side by
side, and writes a results bundle (below). Tests are named by id
(`network/link-pull`) or by category (`network`); each runs once, in the
order given. `--preset` runs a preset's tests instead. Tests a machine
cannot run are skipped there with the reason.

`--machine` takes machine names and pools, separated by commas. A machine
named on its own runs every test. A pool is written `pool:` and its name,
such as `--machine qemu,pool:optiplex`: the pool's machines, but its
spares (`spare = true`, see [Machine files](machine-files.md#pools)),
share the pool's tests out. A machine cannot be named both on its own and
as a member of a picked pool, and a pool is picked once.

A pool has one queue of tests. Each of its machines takes the next test
when it is free, longest first by how long the test took last time on the
pool's machines, or by its time limit when it has not run there before, so
that no machine is left with a long test at the end. Each test runs once,
on one machine: `results` and the bundle say which (`machine`), and a test
that shares a boot does so with the test before it on that machine, as
anywhere. A test the pool's machines cannot run (they lack a control it
needs, or it needs a person and `--with-person` is not given) is skipped
once for the pool, with `pool:<name>` as its machine.

A machine of a pool that will not start, or that is benched by its health
check, takes no more tests, and the rest of the pool carries on with the
queue. A `machine-out` event says so, with the reason. Only when every
machine of the pool is out are its remaining tests skipped, each with
`no machine of pool:<name> was left to run this test:` and why each
machine left. The same holds for a test taken by a machine that was then
benched: it is an `error` as on any machine, and is not run again. run.json
lists each pool and its members (`pools`), and its machines are every
machine that took part.

A Needs you test (one with a control a person does on that machine) is
skipped unless `--with-person` is given. With it, each step a person
does is reported as `NEEDS YOU` (a `person-needed` event), and the run
waits until someone runs the `done` command the line gives, or presses
Done on the runner page. If no one does by the test's time limit, the
test is skipped, not failed. Only give `--with-person` when someone is
at the machines.

`--resume <run>` starts a new run of the tests that run did not finish,
and only those: the ones skipped because the run was stopped (before or
during the test) or that its process died in or never reached. It runs
them on the machines they were left on (a machine with nothing left is
not started), with the options the first run had, which its run.json
records: `--with-person`, the preset and the kits (`--linux`, `--redteam`, `--fonts`,
`--browsers`, `--stick`, `--dvd`, `--ob`); a kit given now fills in one the
run had none of. Give no tests, `--all`, `--preset` or `--machine` with
it. The new run's run.json names the run it resumes (`resumes`), and
`--resume` refuses, saying why, when:

- the run is still going (`dukovo-lab status` shows it);
- every test of the run ran to its end;
- `--build` or `--image` is not the run's own build and image:
  `dukovo-lab runs --json` shows each run's build and image. Check out the
  run's build and rebuild the image (`ob lab`) to resume it. A run given no
  `--build` has only its image's path to go by, so its image is also
  refused when the file is newer than the run (`ls -l` shows its time).

The first run's bundle is not changed. Resuming a run twice runs its
unfinished tests twice.

A test that fails (not one that errors or is skipped) is run once more
before the run ends: on another machine of its pool, a spare
(`spare = true`) first, else on the same machine on a fresh boot. results.json
keeps the first try as the result and records the second beside it as
`rerun` (its machine, result, reason, time and log, in `logs/<machine>/rerun/`),
so counts and findings read only the first. A failure whose rerun passes is
flaky. `rerun-started` and `rerun-ended` events report it. `--no-rerun`
turns it off; schedules do not give it.

`--notify` sends the push notice of `notify.toml` when the run ends (see
[notify](#notify)). A run asked for a notice without that file is
refused before it starts. A notice that cannot be sent is reported on
standard error, which a detached run keeps in its `output.log`, and
changes neither the run's results nor its exit status. Scheduled runs
send their notice without it.

A machine runs one run at a time. When a picked machine is busy with
another run, `run` exits 4 and runs nothing.

`--linux` names the folder of a Linux kit: `kit.toml` and the kernel and
initramfs it names, which performance tests' `bench` steps measure
Dukovo against (see [test files](test-files.md#performance-tests)).
`ob lab` builds the kit and passes it. Without one, tests with bench
steps are skipped. A kit's `kit.toml`:

```toml
kernel = "vmlinuz"            # beside kit.toml
initrd = "linux-bench.cpio"   # beside kit.toml; its first program runs the benchmarks
cmdline = "console=ttyS0 panic=-1 quiet mitigations=off"
done = "linux-bench: done"    # what the serial console says once every benchmark has run
```

`--redteam` names the folder of a red-team kit: the Linux that the
attacker of a test with `attacker = "linux"` boots from, in place of a
Dukovo guest (see [test files](test-files.md#the-red-team-kit)). `ob lab
redteam-kit` builds it. Without one, those tests are skipped, saying so;
tests without `attacker = "linux"` keep the Dukovo guest as their
attacker, whether or not a kit is given, since they were written for
Dukovo's own tools. The kit's `kit.toml`:

```toml
kernel = "vmlinuz"            # beside kit.toml
initrd = "redteam.cpio"       # beside kit.toml
cmdline = "console=ttyS0 panic=-1"
user = "root"                 # optional: the account steps run as (default root)
ready = "redteam: sshd up"    # optional: the serial line that says SSH is up
memory = "3G"                 # optional: the attacker's memory instead of the machine's
```

The initramfs is unpacked into the attacker's memory, so a kit that
unpacks to more than the machine's own `memory` holds sets its own.

`--fonts` names a folder of PSF fonts (`.psf`, `.psfu`): the console's
own, which `screen` and `screen_gone` steps read the screen's text with
(see [test files](test-files.md#reading-the-screen)). `ob lab` passes
Dukovo's, `kernel/lib/fbcon/fonts`. Without it, tests that read the
screen are skipped.

`--browsers` names the browser kit: a folder with `node_modules/playwright`
and `node_modules/axe-core`, and `browsers` when the engines are the kit's
own, which `browser` steps drive web pages with (see [test
files](test-files.md#browser-tests)). `ob lab` builds it in
`out/lab-browsers` and passes it. Without it, tests with browser steps are
skipped. `dukovo-lab` does not look inside the folder: the runner checks
it for each engine, and skips an engine the kit lacks.

`--stick` names the install stick's image, which tests with `boot =
"stick"` boot from (see [test files](test-files.md#installer-tests)),
and `--dvd` the install DVD's ISO, for tests with `boot = "dvd"`. `ob
lab` builds both beside the lab image and passes them. Without one, its
tests are skipped.

`--ob` names Dukovo's `ob`, whose `ob disc-drive` holds a test's music
CD in a QEMU machine's emulated USB CD drive (see [test
files](test-files.md#optical-drive-tests)). `ob lab` passes its own.

Without `--json` it prints a line as each test starts and ends. With
`--json` it prints one JSON object per line as things happen, each with
its time in `at`:

```json
{"event":"run-started","id":"20261107-153012-4f2a","bundle":"/home/chase/.local/share/dukovo-lab/runs/2026-11-07/20261107-153012-4f2a","machines":["qemu"],"tests":["boot/reaches-ready"],"at":"2026-11-07T15:30:12Z"}
{"event":"test-started","machine":"qemu","test":"boot/reaches-ready","at":"..."}
{"event":"step-ended","machine":"qemu","test":"boot/reaches-ready","step":"run `whoami`, exit status 0, output has \"lab\"","result":"pass","milliseconds":800,"note":"$ whoami\nlab","lab_pc_load":0.31,"at":"..."}
{"event":"person-needed","machine":"optiplex","test":"network/cable-pull","step":"pull the network cable","done":"dukovo-lab done 20261107-153012-4f2a --machine optiplex","at":"..."}
{"event":"machine-out","machine":"optiplex-3","reason":"optiplex-3 was benched: its health check failed twice (...)","at":"..."}
{"event":"test-ended","machine":"qemu","test":"boot/reaches-ready","result":"pass","reason":null,"milliseconds":41000,"boot":1,"log":"logs/qemu/boot-reaches-ready.log","screenshots":[],"at":"..."}
{"event":"run-ended","id":"20261107-153012-4f2a","bundle":"...","summary":{"passed":1,"failed":0,"skipped":0,"errors":0},"stopped":false,"findings":[],"at":"..."}
```

`result` is `pass`, `fail` (a check failed or the time limit passed: a
finding), `skip` (with the reason) or `error` (the harness or the machine
broke, so the result says nothing about Dukovo). A step's `note` is cut
at 2000 bytes; the test's log has it whole. `lab_pc_load` is the lab PC's
load per CPU as the step ended (Linux only; left out elsewhere).

Some reasons are worth knowing:

- `stalled: no output for 10 min` (or `3 min`) is a `fail`: a `run`
  step's command printed nothing, and a `serial` step saw no new console
  line, for the stall time (ten minutes for `run`, three for `serial`,
  unless the test's `stall` says otherwise: see
  [test files](test-files.md#steps-that-go-silent)), instead of waiting out
  the test's time limit. The lines below it give the
  console's last line and what the command had printed. The same hang is
  one finding.
- `health check failed after the fresh boot: ...` is an `error`: a real
  machine's root is too small or its clock is off, so the test says
  nothing about Dukovo. The reason names the check and what to do.
- `<machine> was benched: its health check failed twice (...)` is a
  `skip`: the machine failed two fresh boots' health checks in a row, and
  its remaining tests are not run. The other machines carry on, and the
  tests a pool's benched machine did not take are taken by the rest of the
  pool. See
  [Real machines](real-machines.md#the-health-check). More kinds of event may be
added, so skip the ones you do not know. The same lines go to
`events.jsonl` in the bundle, with or without `--json`.

With `--detach`, `run` checks everything, starts the run in a process of
its own and returns at once, printing the run's id (with `--json`:
`{"event":"run-detached","id":"...","bundle":"...","pid":1234}`). The run
carries on after the terminal closes. Follow it with `status`. If the run
fails to start (another run took a machine first, say), `run --detach`
exits as the run did, with its message.

A run tests one image from its first boot to its last: it gives the
image a second name in its work folder and boots that, so rebuilding the
image (another `ob lab run`, say) while it goes does not change it.

## status

```
dukovo-lab status [<run>] [--wait]
```

How a run is going, or how it went: the run named, else the newest run
going, else the newest run. Its state is `running`, `ended`, `stopped`
or `died` (its process ended part way: killed, crashed, or the PC lost
power). With `--wait`, it waits until the run is no longer running, then
answers. With `--json`:

```json
{"id": "...", "state": "running", "bundle": "...", "build": "0.13.0 fd2a9e8", "preset": null,
 "started": "...", "ended": null,
 "counts": {"passed": 14, "failed": 2, "skipped": 0, "errors": 0, "running": 1, "queued": 20},
 "machines": [{"name": "qemu", "running": "network/link-pull", "counts": {...}}],
 "tests": [{"test": "boot/reaches-ready", "machine": "qemu", "state": "pass", "reason": null,
            "milliseconds": 41000, "started": "..."}],
 "waiting": [{"machine": "optiplex", "test": "...", "step": "pull the network cable", "done": "dukovo-lab done ..."}],
 "left_seconds": 2460, "recent": [<the last 40 events>], "output": null}
```

A test's `state` is `queued`, `running`, or its result. A test of a pool
that no machine has taken yet is `queued` with `pool:<name>` as its
machine, and moves to the machine that takes it. `left_seconds`
is an estimate from how long the same tests took on the same machines
the last time they ran to their end, and is null when any of them has
not run there. `output` holds a dead detached run's last messages of its
own.

`status` exits as the run did (0, 1 or 3, and 3 for a run that died),
or 0 while it is running, so `run --detach` then `status --wait` exits
as a plain `run` would have.

## stop

```
dukovo-lab stop [<run>]
```

Stops a run (default: the newest going) at the next place a step can be
cut short, within a second or so: a command, a wait for serial text or
for a person. The test it was in and every test after it are skipped
with the reason, its machines are powered off, its bundle is written,
and run.json says `"stopped": true`. `stop` waits until all that is
done, up to two minutes, then answers as `status` does.

## done

```
dukovo-lab done [<run>] --machine <name>
```

Tells a run that a person did the step it waits for on that machine.
Only a step it is waiting for counts. With `--json`:
`{"done": true, "run": "...", "machine": "..."}`.

## results

```
dukovo-lab results [<run>] [--failed]
```

A run's results: each test on each machine, its result and why, and the
log to read for a failure. `--failed` shows only failures and errors.
With `--json`: `{"state", "run": <run.json>, "bundle", "results":
[<results.json's results>]}`. A run still going, or one that died, has
the results of the tests that ended. It exits as `status` does.

## runs

```
dukovo-lab runs [--limit <n>]
```

The newest runs (20 unless `--limit` says), newest first, each with its
state, start, build, preset, the schedule that started it (`schedule`,
if one did), machines, test count, summary and whether it was sent to
lab-results (`sent`). With `--json`: `{"runs": [...]}`.

## findings

```
dukovo-lab findings [--run <run>] [--all]
dukovo-lab findings <finding> [--not-a-bug [--note <text>] | --reopen | --class <class>]
```

What failed, grouped: each failure belongs to a *finding*, the same test
failing the same way, so a failure that comes back joins the finding it
made before (with one more run, machine or build) instead of making a
new one. Only `fail` results make findings; an `error` is the lab's
problem, not Dukovo's. A run adds its findings as it ends, and its
`run-ended` line names them.

With no finding named, it lists the open findings (not those marked not
a bug or fixed), most recently seen first; `--all` lists every one and
`--run <run>` those a run failed in. With `--json`: `{"findings":
[...]}`, each as [schema/finding.schema.json](../schema/finding.schema.json)
says. Each id is `f-` and 12 hex digits, the same on any PC.

`findings <finding>` shows one: its state, every run it failed in and
what it said last time. With `--json`, the finding itself.

- `--not-a-bug` says it is not a bug (the test is wrong, a cable was
  out), with why in `--note`. It stays so however often it comes back
  and is not offered to Send, unless asked for by id.
- `--reopen` takes that back: it is new again.
- `--class` gives it a class (`lab`, `dukovo`, `flaky`, `known` or `test`),
  which holds in every run it fails in and survives a Send; `auto` goes
  back to the class the lab works out. `--not-a-bug` and `--reopen` clear
  it.

Each failed or errored result also has a class, which `results --json`
lists in `classes` (with the result's finding): `lab` for an error, then a
person's class, `test` for a finding marked not a bug, `known` for one with a
fix proposed, approved or landed or a link to its fix, `flaky` for a failure
that passed when run again, and `dukovo` for any other failure.

A finding's other states (fix proposed, approved, fixed) are set in
lab-results by the thread that fixes it and the maintainer; each Send
brings them back. A fixed finding that fails again is new.

## send

```
dukovo-lab send <finding>... | --all | --run <run> [--results <dir>]
```

Sends findings, and the runs they failed in that lab-results does not
have yet, to the private dukovo-os/lab-results repository, where a
thread proposes a fix for each. `--all` sends every finding with
something new (and any already there that a person has since marked not
a bug); `--run <run>` sends a run and its findings. It uses a clone of
lab-results on the lab PC, `lab-results` in the store unless
`--results` names another, and the PC's own git and GitHub sign-in: it
starts the clone afresh from lab-results, copies each run's bundle in
(logs compressed), merges each finding with the copy there, commits and
pushes. See [Sending findings](sending.md).

With `--json`: `{"commit": "<sha>" or null, "runs": [...], "findings":
[...]}`; `commit` is null when lab-results already had all of it. It
exits 2 when there is no clone or nothing named exists, and 3 when git
or the network fails; nothing is then marked sent, so the next Send
tries again.

## schedule

```
dukovo-lab schedule
dukovo-lab schedule save <name> <test or category>... | --all | --preset <name>
                     (--at <HH:MM> [--days <mon,...>] | --each-build)
                     [--machine <a,b>] [--send | --hold] [--title <text>]
dukovo-lab schedule remove | pause | resume | start <name>
dukovo-lab schedule install
dukovo-lab schedule tick
```

Schedules run tests by themselves: at a time of day in the lab's zone
(Mountain Time), on the days `--days` names or every day, or on each new
build on Dukovo's main (`--each-build`), on the lab's own Dukovo checkout
and with the runner page closed. See [Schedules](schedules.md).

- With no action, it lists the schedules: each with what it runs, when
  in words (`"when": "Every day, 1:00 AM"`), its next time (`next`, UTC, and `next_text`, such as "Tomorrow, 1:00
  AM", in the lab's zone),
  what its start is doing if one is going (`going`), and how the last
  one went (`last`: its run, the commit of main it tested, `result`
  passed, failed, broke, stopped, missed or error, its findings and
  whether they were sent). With `--json`: `{"zone": "America/Denver",
  "zone_short": "MT", "timer": true, "schedules": [...]}`; `timer` says
  whether the timer is installed.
- `save` makes a schedule, or changes the one of that name (a paused one
  stays paused). Its findings are sent to lab-results when it ends
  unless `--hold` says to hold them. `--machine` defaults to `qemu`. A
  time already passed today waits for tomorrow.
- `start` starts it now, in the background, as its time would; it exits
  4 if it is already going. `pause` and `resume` stop and restart its
  timing; times passed while paused are not made up.
- `install` puts the systemd timer in place and turns it on; `ob lab`
  does this each time. `tick` is what the timer runs each minute: it
  starts the schedules that are due, and is safe to run by hand.

## ui

```
dukovo-lab ui [--image <file>] [--build <text>] [--linux <dir>] [--redteam <dir>]
              [--fonts <dir>] [--browsers <dir>] [--stick <file>] [--dvd <file>] [--ob <file>]
              [--port <n>] [--share <address>] [--new-share-key] [--no-open]
```
Serves the runner page to a browser on this PC, at
`http://127.0.0.1:7420/` unless `--port` says otherwise, and opens it
when there is a desktop (not with `--no-open`). It prints the address to
open, which carries a key made anew each time; without it the page does
not open. Ctrl+C closes the page; runs started from it carry on. Without
`--image` the page shows tests and runs but cannot start runs; `ob lab
ui` gives the image. Runs started from the page get `--linux`,
`--redteam`, `--fonts`, `--browsers`, `--stick`, `--dvd` and `--ob` when they were given. See [the runner page](runner-page.md).

`--share <address>` also serves the page read only, for a phone, on one
address of this PC at the same port: the lab PC's Tailscale address,
whose network holds only the owner's own devices. It prints a second
address, `http://<address>:<port>/?key=…#phone`, with a key of its own:
the share key, made once and kept in `share_key` in the configuration
folder (mode 600) so a notice's link keeps working. `--new-share-key`
replaces it. That listener answers GET requests alone and
refuses every other method with 403; the full page's key does not open
it, and its key does not open the full page. `0.0.0.0` and `::`, which
mean every network, are refused. An address that is not Tailscale's,
a LAN or public one, is allowed with a warning: anyone who reaches it
and learns the key can read the lab's status, so do not use one (see
[Sharing read only](runner-page.md#sharing-read-only)).

## notify

```
dukovo-lab notify --test
```

Sends a test notice to the ntfy topic in `notify.toml` in the
configuration folder, so the owner can check that it reaches their phone
(see [Notices](schedules.md#notices)). Exits 2 when there is no
`notify.toml` or no `--test`, and 3 when the notice could not be sent;
the answer says why. With `--json`: `{"sent": true}`.

## key

```
dukovo-lab key
```

Makes the lab's SSH key, `lab_ed25519` in the configuration folder, and
the lab password, `lab_password` beside it, each if it is not there, and
prints the public key's path. `ob image --lab` puts the public key and
the password's hash in the image; the private key and the password
never leave the lab PC. The password is 32 random letters and digits,
in a file only its owner reads; steps with `root = true` give it to
elevate ([test files](test-files.md#running-as-root)). With `--json`:
`{"public_key": "<path>", "made": true, "password_file": "<path>", "made_password": false}`.

## Exit status

| Status | Meaning |
| --- | --- |
| 0 | Every test that ran passed. Tests may have been skipped. |
| 1 | A test failed. |
| 2 | A bad argument, test file or machine file; nothing ran. |
| 3 | The harness or a machine broke, so some results say nothing about Dukovo. |
| 4 | A machine the run needs, a pool's machines included, is busy with another run; nothing ran. |

With `--json`, statuses 2, 3 and 4 also print `{"error": "<what>", "status": <n>}`.

## Results bundles

Each run writes `runs/<YYYY-MM-DD>/<run id>/` under the store, laid out
as dukovo-os/lab-results keeps runs:

```
run.json                  what ran: build, image, machines, tests, preset, start and end
results.json              each test's result on each machine, step by step, with the lab PC's load at each step
events.jsonl              what happened as it happened: run's JSON lines, every one
logs/<machine>/           serial-<boot>.log, qemu-<boot>.log, <category>-<test>.log, and the traces of failed browser steps
screenshots/<machine>/    <category>-<test>-<name>.png, and .txt: the screen's text, with --fonts
```

A run a schedule started names it in run.json (`schedule`), and a run
that resumes another names that (`resumes`). run.json also records the
options the run was given (`options`: `with_person` and the kits' folders
and files), which a resume gives again. Once a run
is sent, its bundle also holds `sent.json` (when, the commit,
and which findings), which stays on the lab PC. Findings live beside the
runs, in `findings/<id>.json` in the store.

[schema/run.schema.json](../schema/run.schema.json),
[schema/results.schema.json](../schema/results.schema.json) and
[schema/finding.schema.json](../schema/finding.schema.json) describe the
JSON. Times are UTC, in RFC 3339.

Each step in results.json has `lab_pc_load`, the lab PC's one-minute load
average divided by its CPU count, read from `/proc/loadavg` as the step
ended (`0.31`; absent where the lab PC has no `/proc/loadavg`). Browser
and bench steps run their side of a test on the lab PC, so a step of
either kind that ends with the load above 0.75 says in its note, and in
the test's log, "the lab PC was busy (load 0.92 per CPU), so this step's
times may be slow". The load never changes a result.

## Troubleshooting

| Symptom | Cause and check |
| --- | --- |
| A test fails at "boot afresh and wait until booted" with the ready line missing | The image did not boot. Read the boot's `serial-<n>.log` in the bundle; an empty one means the firmware never reached Dukovo (check `--image` and the machine's `firmware`). |
| "booted, but SSH did not answer as the lab account" | The image was not built with `ob image --lab`, or was built before `dukovo-lab key` made the current key. Rebuild the image. |
| Every test on a machine is `error` with "did not start" | The driver could not start: QEMU, qemu-img or OVMF is missing, or `kvm = "on"` without `/dev/kvm`. The reason says which. |
| "connecting to QMP" errors | QEMU stopped before it was ready. `qemu-<n>.log` holds its command line and its own message. |
| A test passes on KVM and fails on time without it | QEMU's emulator is several times slower: raise the machine's `tcg_slowdown`, not the test's `time`. |
| `run` exits 2 saying a test runs commands and the lab has no key | Run `dukovo-lab key`, then rebuild the image with `ob image --lab`. |
| `run` exits 2 saying a step runs as root and the lab has no password | The lab key was made before the lab password existed. Run `dukovo-lab key`, which makes the password beside the key, then rebuild the image (`ob lab` does both). |
| A root step fails with "Sorry, try again." | The image holds another password's hash: it was built before the current `lab_password`, or on another PC. Rebuild it. |
| Performance tests are skipped saying the run was given no Linux kit | `ob lab` builds the kit before the run and prints `note: performance tests will skip: ...` when it cannot, with the reason: usually no network to fetch the Linux kernel, or `dpkg-deb` or `depmod` missing. |
| A bench step is skipped saying QEMU's emulator made the numbers | The machine has no KVM. The step still keeps both tables in its note; give the PC KVM (`/dev/kvm`) for numbers worth holding to Linux's. |
| A bench or browser step's note says the lab PC was busy | The lab PC's load was above 0.75 per CPU when the step ended, so its times may be slow. The result is as the step's checks left it; run again when the lab PC is quiet before sending a slow result. |
| A test fails "stalled: no output for 10 min" (or 3 min) | A command printed nothing and the console showed no new line for the stall time: it hung, or it is quiet by nature. The reason has the console's last line. Give the test or the step a longer `stall`, or `"off"`, when it is quiet by design. |
| A test is an `error` with "health check failed after the fresh boot" | A real machine's root has less than 1 GiB free or its clock is more than 5 s off the lab PC's. The reason says which and what to do; see [Real machines](real-machines.md#the-health-check). |
| Tests are skipped saying a machine "was benched" | Its health check failed on two fresh boots in a row. Put the machine right (the earlier tests' reasons say how), then run those tests again. |
| A bench step fails "slower than Linux" | A finding: the rows marked `over` take more than `limit` times Linux's time. The note has both tables; `logs/<machine>/linux-boot-<n>.log` has Linux's run. Run again to rule out a busy PC before sending it. |
| Tests that read the screen are skipped saying the run was given no console fonts | Give `--fonts` with Dukovo's `kernel/lib/fbcon/fonts`; `ob lab` does. |
| A `screen` step fails though the text is plainly on the screenshot | The step reads cells, each within one row: text that wraps past the row's end, or a word the cursor sits on, does not match. The `.txt` beside the screenshot shows what was read. A screen of `?` means the fonts are not the console's: rebuild with the same checkout as the image. |
| Tests with browser steps are skipped saying the run was given no browser kit | Give `--browsers` with the kit's folder; `ob lab` builds it in `out/lab-browsers` and prints `note: the web console's tests will skip: ...` when it cannot, with the reason: usually no `npm`, or no network to fetch the packages. |
| A browser step is skipped saying the lab PC has no node | Install Node.js 22 or later on the lab PC. |
| An engine is skipped saying it is not in the browser kit | The kit has no copy of that engine: `ob lab` installs all three, but a machine that provides its own through `PLAYWRIGHT_BROWSERS_PATH`, as the cloud build machines do, may have only Chromium. |
| An engine is skipped saying it would not start on the lab PC | A system library the engine needs is missing. On Linux, `sudo npx --prefix out/lab-browsers playwright install-deps` installs them. |
| Installer tests are skipped saying the run was given no stick | Give `--stick` with the install stick's image (`ob image` builds it as `out/dukovo-<version>-x86_64.img`); `ob lab` builds and passes it. |
| Installer tests are skipped saying the run was given no DVD | Give `--dvd` with the install DVD's ISO (`ob image --iso` builds it as `out/dukovo-<version>-x86_64.iso`); `ob lab` builds and passes it. |
| An AMT call fails "tried 3 times" | The network dropped, refused or timed out each of three tries, 2 seconds apart. AMT's own refusals (a `ReturnValue`) are not tried again. Check the lab network and the machine's AMT with `dukovo-lab machines check`. |
| `run --resume` refuses: build or image differs | A resume tests what the first run did. See [run](#run). |
| `run` exits 4: a machine is busy | Another run is using it. `dukovo-lab status` shows that run; wait for it or `dukovo-lab stop` it. |
| `status` says a run `died` | Its process ended part way (killed, crashed, or the PC went down). `results` has what it finished; a detached run's own last messages are in `status`'s `output`. |
| `send` exits 2: there is no lab-results clone | Make it once: `git clone git@github.com:dukovo-os/lab-results.git ~/.local/share/dukovo-lab/lab-results` (`ob lab` does this for you). The lab PC's GitHub account needs push access. |
| `send` exits 3 saying the clone has changes not committed | Someone edited the clone outside `runs/` and `findings/`. `git -C <clone> status` shows what; commit and push it, or drop it, then send again. |
| `send` exits 3 saying it could not push | Nothing was marked sent; the next send tries again. Check the network, and that `git -C <clone> push` works as the PC's GitHub account (`gh auth status`). |
| A schedule's last result is `error` | It could not run: the lab's checkout would not update or the image would not build. `dukovo-lab schedule` says why, and its log (`schedules/<name>.log` in the store) holds what `ob` printed. |
| A schedule never starts | Is the timer on? `systemctl --user status dukovo-lab-schedules.timer`; `dukovo-lab schedule install` (or `ob lab`) puts it in place. `journalctl --user -u dukovo-lab-schedules` shows each tick's problems. |
| A schedule's result is `missed` | The lab PC was off or asleep at its time, for more than 12 hours. |
| `notify --test` exits 3: curl is not installed | Notices are sent with `curl`: `sudo apt install curl` on the lab PC. |
| `notify --test` exits 3: curl said ... | The topic address in `notify.toml` is wrong, or the lab PC cannot reach the server. curl's reason follows: for example "Could not resolve host" is a network or spelling problem, and "returned error: 404" is a server that does not take the address. |
| A scheduled run's notice did not come | The schedule's log (`schedules/<name>.log` in the store) says why it could not be sent; with no line there, `notify.toml` is missing. |
| The runner page says its key has changed | `dukovo-lab ui` was started again: open the address it printed this time. |
