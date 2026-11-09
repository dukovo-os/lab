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
| `--config <dir>` | `~/.config/dukovo-lab` | Machine files (`machines/`), the schedules (`schedules.json`) and the lab key. |
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
object as in `list`, less `last_milliseconds`.

## machines

```
dukovo-lab machines
```

Each machine, its file and who does each control. With `--json`:
`{"machines": [{"name", "about", "file", "driver", "ready", "controls":
{"power": "driver", ...}, "qemu": {...}}]}`.

## run

```
dukovo-lab run <test or category>... | --all | --preset <name> --image <file>
               [--machine <a,b>] [--build <text>] [--linux <dir>] [--fonts <dir>]
               [--stick <file>] [--dvd <file>] [--with-person] [--detach]
```

Runs the tests on each machine (default `qemu`), the machines side by
side, and writes a results bundle (below). Tests are named by id
(`network/link-pull`) or by category (`network`); each runs once, in the
order given. `--preset` runs a preset's tests instead. Tests a machine
cannot run are skipped there with the reason.

A Needs you test (one with a control a person does on that machine) is
skipped unless `--with-person` is given. With it, each step a person
does is reported as `NEEDS YOU` (a `person-needed` event), and the run
waits until someone runs the `done` command the line gives, or presses
Done on the runner page. If no one does by the test's time limit, the
test is skipped, not failed. Only give `--with-person` when someone is
at the machines.

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

`--fonts` names a folder of PSF fonts (`.psf`, `.psfu`): the console's
own, which `screen` and `screen_gone` steps read the screen's text with
(see [test files](test-files.md#reading-the-screen)). `ob lab` passes
Dukovo's, `kernel/lib/fbcon/fonts`. Without it, tests that read the
screen are skipped.

`--stick` names the install stick's image, which tests with `boot =
"stick"` boot from (see [test files](test-files.md#installer-tests)),
and `--dvd` the install DVD's ISO, for tests with `boot = "dvd"`. `ob
lab` builds both beside the lab image and passes them. Without one, its
tests are skipped.

Without `--json` it prints a line as each test starts and ends. With
`--json` it prints one JSON object per line as things happen, each with
its time in `at`:

```json
{"event":"run-started","id":"20261107-153012-4f2a","bundle":"/home/chase/.local/share/dukovo-lab/runs/2026-11-07/20261107-153012-4f2a","machines":["qemu"],"tests":["boot/reaches-ready"],"at":"2026-11-07T15:30:12Z"}
{"event":"test-started","machine":"qemu","test":"boot/reaches-ready","at":"..."}
{"event":"step-ended","machine":"qemu","test":"boot/reaches-ready","step":"run `whoami`, exit status 0, output has \"lab\"","result":"pass","milliseconds":800,"note":"$ whoami\nlab","at":"..."}
{"event":"person-needed","machine":"optiplex","test":"network/cable-pull","step":"pull the network cable","done":"dukovo-lab done 20261107-153012-4f2a --machine optiplex","at":"..."}
{"event":"test-ended","machine":"qemu","test":"boot/reaches-ready","result":"pass","reason":null,"milliseconds":41000,"boot":1,"log":"logs/qemu/boot-reaches-ready.log","screenshots":[],"at":"..."}
{"event":"run-ended","id":"20261107-153012-4f2a","bundle":"...","summary":{"passed":1,"failed":0,"skipped":0,"errors":0},"stopped":false,"findings":[],"at":"..."}
```

`result` is `pass`, `fail` (a check failed or the time limit passed: a
finding), `skip` (with the reason) or `error` (the harness or the machine
broke, so the result says nothing about Dukovo). A step's `note` is cut
at 2000 bytes; the test's log has it whole. More kinds of event may be
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

A test's `state` is `queued`, `running`, or its result. `left_seconds`
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
dukovo-lab findings <finding> [--not-a-bug [--note <text>] | --reopen]
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
dukovo-lab ui [--image <file>] [--build <text>] [--linux <dir>] [--fonts <dir>]
              [--stick <file>] [--dvd <file>] [--port <n>] [--no-open]
```

Serves the runner page to a browser on this PC, at
`http://127.0.0.1:7420/` unless `--port` says otherwise, and opens it
when there is a desktop (not with `--no-open`). It prints the address to
open, which carries a key made anew each time; without it the page does
not open. Ctrl+C closes the page; runs started from it carry on. Without
`--image` the page shows tests and runs but cannot start runs; `ob lab
ui` gives the image. Runs started from the page get `--linux`,
`--fonts`, `--stick` and `--dvd` when they were given. See [the runner page](runner-page.md).

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
| 4 | A machine the run needs is busy with another run; nothing ran. |

With `--json`, statuses 2, 3 and 4 also print `{"error": "<what>", "status": <n>}`.

## Results bundles

Each run writes `runs/<YYYY-MM-DD>/<run id>/` under the store, laid out
as dukovo-os/lab-results keeps runs:

```
run.json                  what ran: build, image, machines, tests, preset, start and end
results.json              each test's result on each machine, step by step
events.jsonl              what happened as it happened: run's JSON lines, every one
logs/<machine>/           serial-<boot>.log, qemu-<boot>.log, <category>-<test>.log
screenshots/<machine>/    <category>-<test>-<name>.png, and .txt: the screen's text, with --fonts
```

A run a schedule started names it in run.json (`schedule`). Once a run
is sent, its bundle also holds `sent.json` (when, the commit,
and which findings), which stays on the lab PC. Findings live beside the
runs, in `findings/<id>.json` in the store.

[schema/run.schema.json](../schema/run.schema.json),
[schema/results.schema.json](../schema/results.schema.json) and
[schema/finding.schema.json](../schema/finding.schema.json) describe the
JSON. Times are UTC, in RFC 3339.

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
| A bench step fails "slower than Linux" | A finding: the rows marked `over` take more than `limit` times Linux's time. The note has both tables; `logs/<machine>/linux-boot-<n>.log` has Linux's run. Run again to rule out a busy PC before sending it. |
| Tests that read the screen are skipped saying the run was given no console fonts | Give `--fonts` with Dukovo's `kernel/lib/fbcon/fonts`; `ob lab` does. |
| A `screen` step fails though the text is plainly on the screenshot | The step reads cells, each within one row: text that wraps past the row's end, or a word the cursor sits on, does not match. The `.txt` beside the screenshot shows what was read. A screen of `?` means the fonts are not the console's: rebuild with the same checkout as the image. |
| Installer tests are skipped saying the run was given no stick | Give `--stick` with the install stick's image (`ob image` builds it as `out/dukovo-<version>-x86_64.img`); `ob lab` builds and passes it. |
| Installer tests are skipped saying the run was given no DVD | Give `--dvd` with the install DVD's ISO (`ob image --iso` builds it as `out/dukovo-<version>-x86_64.iso`); `ob lab` builds and passes it. |
| `run` exits 4: a machine is busy | Another run is using it. `dukovo-lab status` shows that run; wait for it or `dukovo-lab stop` it. |
| `status` says a run `died` | Its process ended part way (killed, crashed, or the PC went down). `results` has what it finished; a detached run's own last messages are in `status`'s `output`. |
| `send` exits 2: there is no lab-results clone | Make it once: `git clone git@github.com:dukovo-os/lab-results.git ~/.local/share/dukovo-lab/lab-results` (`ob lab` does this for you). The lab PC's GitHub account needs push access. |
| `send` exits 3 saying the clone has changes not committed | Someone edited the clone outside `runs/` and `findings/`. `git -C <clone> status` shows what; commit and push it, or drop it, then send again. |
| `send` exits 3 saying it could not push | Nothing was marked sent; the next send tries again. Check the network, and that `git -C <clone> push` works as the PC's GitHub account (`gh auth status`). |
| A schedule's last result is `error` | It could not run: the lab's checkout would not update or the image would not build. `dukovo-lab schedule` says why, and its log (`schedules/<name>.log` in the store) holds what `ob` printed. |
| A schedule never starts | Is the timer on? `systemctl --user status dukovo-lab-schedules.timer`; `dukovo-lab schedule install` (or `ob lab`) puts it in place. `journalctl --user -u dukovo-lab-schedules` shows each tick's problems. |
| A schedule's result is `missed` | The lab PC was off or asleep at its time, for more than 12 hours. |
| The runner page says its key has changed | `dukovo-lab ui` was started again: open the address it printed this time. |
