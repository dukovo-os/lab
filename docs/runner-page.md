<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# The runner page

The runner page is the lab's control panel: pick machines and tests, run
them, watch the run, read what failed and send it on. It runs on the lab
PC and only a browser on that PC can open it.

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
- **Findings.** Each failed test with its finding: new, not a bug, fix
  proposed, approved or fixed, how often and where it failed before, and
  whether it passes again somewhere. Open one for why, its steps, its
  test log, the boot's serial log and its screenshots. Then:
  - **Send this finding** sends it, with its runs and logs, to the
    private lab-results repository, where a thread proposes a fix and
    nothing is fixed until the maintainer approves it.
  - **Run again** runs that test on that machine.
  - **Not a bug** asks, in place, why (the test is wrong, a cable was
    out; it can stay empty), then marks it so: it is not offered to send
    again. **Reopen** takes that back.

  A test that broke rather than failed shows as a **lab problem**: the
  harness or the machine broke, so it says nothing about Dukovo and is
  never sent. Under the list, **Send** sends every new finding of the
  run, and **Send all new** those of other runs too. Send needs a
  lab-results clone ([Sending findings](sending.md)); `ob lab` makes it.
- **Past runs.** Every run, newest first, with its findings and whether
  they were sent. Click one to show it above.

Schedules come in a later update of the lab.

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
| Findings | `results <run>`, `findings --all` |
| Send this finding | `send <finding>` |
| Send, Send all new | `send --run <run>`, `send --all` |
| Not a bug, Reopen | `findings <finding> --not-a-bug [--note <text>]`, `findings <finding> --reopen` |
| Past runs | `runs` |
| Tests, Machines | `list`, `machines` |

## Safety

The page listens on 127.0.0.1 only, answers only to that address and
`localhost`, needs the printed key (which it keeps in a cookie), and
takes actions only from its own script. It serves its own files, the
commands' answers and files inside results bundles; never anything else
on the PC, and never the lab key. Send pushes only bundles and findings,
as `dukovo-lab send` does.

The lab PC is meant to have one account. If it has others, know that
opening the browser hands it the address, key and all, and other
accounts can see a program's arguments while it runs; give `--no-open`
and paste the printed address into the browser yourself.
