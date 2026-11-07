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
| `--config <dir>` | `~/.config/dukovo-lab` | Machine files (`machines/`) and the lab key. |
| `--store <dir>` | `~/.local/share/dukovo-lab` | Results bundles (`runs/`) and the run's work files while it runs. |
| `--json` | | Answer in JSON. |

## list

```
dukovo-lab list [<test or category>...] [--machine <a,b>]
```

The tests by category, with their time limits, and a line for each
machine a test does not simply run on. With `--json`:

```json
{"tests": [{"id": "drivers/link-pull", "category": "drivers", "about": "...", "time": "3m",
  "fresh": false, "file": "lab/tests/drivers/link-pull.toml",
  "controls": ["power", "serial", "link", "ssh"],
  "machines": [{"machine": "qemu", "fit": "runs", "controls": []}],
  "steps": [{"step": "pull the network cable", "control": "link"}],
  "last_milliseconds": {"qemu": 61500}}],
 "categories": [{"id": "boot", "title": "Boot and install"}],
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
               [--machine <a,b>] [--build <text>] [--with-person] [--detach]
```

Runs the tests on each machine (default `qemu`), the machines side by
side, and writes a results bundle (below). Tests are named by id
(`drivers/link-pull`) or by category (`drivers`); each runs once, in the
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

Without `--json` it prints a line as each test starts and ends. With
`--json` it prints one JSON object per line as things happen, each with
its time in `at`:

```json
{"event":"run-started","id":"20261107-153012-4f2a","bundle":"/home/chase/.local/share/dukovo-lab/runs/2026-11-07/20261107-153012-4f2a","machines":["qemu"],"tests":["boot/reaches-ready"],"at":"2026-11-07T15:30:12Z"}
{"event":"test-started","machine":"qemu","test":"boot/reaches-ready","at":"..."}
{"event":"step-ended","machine":"qemu","test":"boot/reaches-ready","step":"run `whoami`, exit status 0, output has \"lab\"","result":"pass","milliseconds":800,"note":"$ whoami\nlab","at":"..."}
{"event":"person-needed","machine":"optiplex","test":"drivers/cable-pull","step":"pull the network cable","done":"dukovo-lab done 20261107-153012-4f2a --machine optiplex","at":"..."}
{"event":"test-ended","machine":"qemu","test":"boot/reaches-ready","result":"pass","reason":null,"milliseconds":41000,"boot":1,"log":"logs/qemu/boot-reaches-ready.log","screenshots":[],"at":"..."}
{"event":"run-ended","id":"20261107-153012-4f2a","bundle":"...","summary":{"passed":1,"failed":0,"skipped":0,"errors":0},"stopped":false,"at":"..."}
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
 "machines": [{"name": "qemu", "running": "drivers/link-pull", "counts": {...}}],
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
state, start, build, preset, machines, test count and summary. With
`--json`: `{"runs": [...]}`.

## ui

```
dukovo-lab ui [--image <file>] [--build <text>] [--port <n>] [--no-open]
```

Serves the runner page to a browser on this PC, at
`http://127.0.0.1:7420/` unless `--port` says otherwise, and opens it
when there is a desktop (not with `--no-open`). It prints the address to
open, which carries a key made anew each time; without it the page does
not open. Ctrl+C closes the page; runs started from it carry on. Without
`--image` the page shows tests and runs but cannot start runs; `ob lab
ui` gives the image. See [the runner page](runner-page.md).

## key

```
dukovo-lab key
```

Makes the lab's SSH key, `lab_ed25519` in the configuration folder, if
it is not there, and prints the public key's path. `ob image --lab` puts
that public key in the image; the private key never leaves the lab PC.
With `--json`: `{"public_key": "<path>", "made": true}`.

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
screenshots/<machine>/    <category>-<test>-<name>.png
```

[schema/run.schema.json](../schema/run.schema.json) and
[schema/results.schema.json](../schema/results.schema.json) describe the
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
| `run` exits 4: a machine is busy | Another run is using it. `dukovo-lab status` shows that run; wait for it or `dukovo-lab stop` it. |
| `status` says a run `died` | Its process ended part way (killed, crashed, or the PC went down). `results` has what it finished; a detached run's own last messages are in `status`'s `output`. |
| The runner page says its key has changed | `dukovo-lab ui` was started again: open the address it printed this time. |
