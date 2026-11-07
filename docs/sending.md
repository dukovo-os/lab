<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Sending findings

A finding is one way a test fails, with every run it failed in
([`findings`](commands.md#findings)). Send puts findings, and the runs
they came from, in **dukovo-os/lab-results**, a private repository. A
thread reads each new finding there, finds the cause and proposes a fix
as a pull request in Dukovo; nothing is fixed until the maintainer
approves it. The finding's state there moves on (fix proposed, approved,
fixed), and the next Send brings that back to the lab PC.

## Setting it up

Send uses a clone of lab-results on the lab PC and the PC's own git and
GitHub sign-in; dukovo-lab holds no GitHub password or token. In a Dukovo
checkout, `./ob lab` makes the clone for you. By hand, once:

```sh
git clone git@github.com:dukovo-os/lab-results.git ~/.local/share/dukovo-lab/lab-results
```

The PC's GitHub account needs push access to lab-results, and git needs
a name and email to commit with (`git config --global user.name`, and
`user.email`). A clone elsewhere works too: give `--results <dir>` to
`send` and to `ui`.

## Sending

On the runner page: Send this finding, Send (every new finding of the
run) or Send all new. From a terminal or an agent:

```sh
dukovo-lab send f-3a9c51d2e07b          # one finding, and its runs not sent yet
dukovo-lab send --run 20261107-153012-4f2a   # a run and its findings
dukovo-lab send --all                   # every finding with something new
```

Each send:

1. Checks the clone is on `main` with nothing left uncommitted, and
   pulls.
2. Copies in each run's bundle that lab-results does not have yet, as
   `runs/<date>/<run>/`: `run.json`, `results.json`, `events.jsonl`,
   `logs/` (each `.log` compressed to `.log.gz`) and `screenshots/`.
3. Merges each finding with the copy in lab-results, as
   `findings/<id>.json`: every run either side has, and the state
   lab-results moved it to, unless a person on the lab PC has since said
   it is not a bug.
4. Commits, as one commit naming the findings and runs, and pushes to
   `main`. If someone pushed first, it takes their commits and pushes
   once more.
5. Notes in the store what went: each finding's runs are marked sent,
   and each bundle gets `sent.json`.

When lab-results already has everything, nothing is committed. If the
push fails, the commit waits in the clone and goes with the next send;
the store still says unsent until then.

## What goes, and what never does

Only what step 2 names, from the bundles, and the findings. A bundle's
files are copied by name, and links are never followed, so nothing else
on the PC can go. The lab key is never in a bundle, so it never goes.
`sent.json` and the store's other files stay on the lab PC.

Findings are about Dukovo and may be security findings, which stay
private until fixed: that is why lab-results is private, and why
nothing is sent anywhere else.
