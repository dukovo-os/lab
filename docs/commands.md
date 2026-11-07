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
  "machines": [{"machine": "qemu", "fit": "runs", "controls": []}]}]}
```

`fit` is `runs`, `needs-person` (with the controls a person does) or
`lacks` (with the controls the machine cannot do).

## show

```
dukovo-lab show <test>
```

One test: what it checks, its file, its time limit, its controls, its
steps in words and how it fits each machine. With `--json`, the same
object as `list`, plus `steps`: `[{"step": "pull the network cable",
"control": "link"}]`.

## machines

```
dukovo-lab machines
```

Each machine, its file and who does each control. With `--json`:
`{"machines": [{"name", "about", "file", "driver", "ready", "controls":
{"power": "driver", ...}, "qemu": {...}}]}`.

## run

```
dukovo-lab run <test or category>... | --all --image <file> [--machine <a,b>] [--build <text>]
```

Runs the tests on each machine (default `qemu`), the machines side by
side, and writes a results bundle (below). Tests are named by id
(`drivers/link-pull`) or by category (`drivers`); each runs once, in the
order given. Tests a machine cannot run are skipped there with the
reason, as are Needs you tests until the runner page can ask a person to
act (lab PR 2).

Without `--json` it prints a line as each test starts and ends. With
`--json` it prints one JSON object per line as things happen:

```json
{"event":"run-started","id":"20261107-153012-4f2a","bundle":"/home/chase/.local/share/dukovo-lab/runs/2026-11-07/20261107-153012-4f2a","machines":["qemu"],"tests":["boot/reaches-ready"]}
{"event":"test-started","machine":"qemu","test":"boot/reaches-ready"}
{"event":"test-ended","machine":"qemu","test":"boot/reaches-ready","result":"pass","reason":null,"milliseconds":41000}
{"event":"run-ended","id":"20261107-153012-4f2a","bundle":"...","summary":{"passed":1,"failed":0,"skipped":0,"errors":0}}
```

`result` is `pass`, `fail` (a check failed or the time limit passed: a
finding), `skip` (with the reason) or `error` (the harness or the machine
broke, so the result says nothing about Dukovo).

`run --detach`, `status --wait`, `stop` and `results` come with the
runner page in lab PR 2.

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

With `--json`, statuses 2 and 3 also print `{"error": "<what>", "status": <n>}`.

## Results bundles

Each run writes `runs/<YYYY-MM-DD>/<run id>/` under the store, laid out
as dukovo-os/lab-results keeps runs:

```
run.json                  what ran: build, image, machines, tests, start and end
results.json              each test's result on each machine, step by step
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
