<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Driving the lab from an agent

The lab is built to be run by an AI agent as easily as by a person.
Everything the runner page does is a `dukovo-lab` command, and every
command answers in JSON, never asks a question and exits with a
documented status ([commands](commands.md)). An agent never needs to read
a web page or scrape a log.

## Rules

The lab's rules apply to agents as to people:

- Never run anything against a machine outside the lab network, and
  never point an attack at anything but a lab machine.
- Never copy the lab key, a bundle's security findings or the lab PC's
  files anywhere but the private results repository.
- Needs you tests are for when a person is at the machines. Do not ask
  for them unless a person has said they are there.
- Only the maintainer approves a fix for a finding.

## Start, follow, report

1. See what there is:

   ```sh
   ./ob lab list --json
   ./ob lab machines --json
   ```

2. Run, following the JSON lines as they come. The first line gives the
   run's id and bundle folder; the last gives the counts.

   ```sh
   ./ob lab run drivers features --machine qemu --json
   ```

3. Read the bundle: `results.json` for each result and its failing step,
   the test's log (`log` in its result) for every step's output, the
   boot's serial log for the kernel's view, and screenshots where there
   are any.

4. Report what failed and why, naming the test, the machine, the build
   (from `run.json`), the failing step and the lines that show it. A
   `fail` is a finding about Dukovo; an `error` is a problem with the lab
   itself and says nothing about Dukovo.

The exit status tells the outcome without parsing anything: 0 passed, 1
a test failed, 2 a bad request, 3 the lab broke, 4 a machine was busy
with another run.

## Long runs

A full run can take hours. Start it detached, then come back to it:

```sh
./ob lab run --all --detach --json        # {"event":"run-detached","id":"<run>",...}
./ob lab status <run> --json              # progress, what is running, time left
./ob lab status <run> --wait --json       # blocks until it ends; exits as run would
./ob lab results <run> --failed --json    # what failed, and the logs to read
./ob lab stop <run>                       # if it must end early
./ob lab runs --json                      # every run so far
```

A person may be watching the same run on the runner page; it shows the
commands it runs, so either of you can pick up where the other is.
