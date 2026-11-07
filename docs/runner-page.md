<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# The runner page

The runner page is the lab's control panel: pick machines and tests, run
them, watch the run, and read what failed. It runs on the lab PC and
only a browser on that PC can open it.

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

- **Machines.** Tick the machines to run on; each runs the same tests at
  the same time. A machine busy with a run says so.
- **Tests,** by category. Tick tests, or a whole category, then Run; or
  use a test's own Run, or a category's Run all. Click a test's name for
  its steps, file, time limit, which machines it runs on and how long it
  took last time. Find narrows the list. A preset picks its tests and
  shows how long it took last time on the picked machines.
- **Needs you.** Tests that need a person on a picked machine are kept
  apart, and run only when ticked. When one of their steps comes up, the
  run panel says what to do on which machine, with a Done button to
  press once it is done.
- **This run.** Progress, counts, an estimate of the time left, each
  machine's tally, a live log of each step, and Stop.
- **Failures.** Each failed test with why, its steps, its test log, the
  boot's serial log and its screenshots, and Run again for that test on
  that machine.
- **Past runs.** Every run, newest first. Click one to show it above.

Sending findings to the results repository, and schedules, come in a
later update of the lab.

## The same as the commands

Every button runs a `dukovo-lab` command, and the bottom of the page
shows the last one, such as `dukovo-lab run --detach drivers/link-pull
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
| Failures | `results <run>` |
| Past runs | `runs` |
| Tests, Machines | `list`, `machines` |

## Safety

The page listens on 127.0.0.1 only, answers only to that address and
`localhost`, needs the printed key (which it keeps in a cookie), and
takes actions only from its own script. It serves its own files, the
commands' answers and files inside results bundles; never anything else
on the PC, and never the lab key.

The lab PC is meant to have one account. If it has others, know that
opening the browser hands it the address, key and all, and other
accounts can see a program's arguments while it runs; give `--no-open`
and paste the printed address into the browser yourself.
