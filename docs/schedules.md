<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Schedules

A schedule runs tests by itself, with the runner page closed. It runs
at a time of day in the lab's zone (Mountain Time), on every day or on
the days it names, or on each new build on Dukovo's main branch. It runs
a preset, tests and categories, or every test, on one or more machines
at once, or on a pool of machines that share the tests out. When it ends, it sends its findings to lab-results, or holds
them for you to send from the page.

Make them on the runner page (Schedules, New schedule) or with
commands:

```sh
dukovo-lab schedule save nightly --preset full --machine qemu,optiplex --at 01:00 --title "Full run"
dukovo-lab schedule save drivers drivers --machine optiplex --at 02:00 --days sun --hold
dukovo-lab schedule save rounds --preset full --machine qemu,pool:optiplex --at 18:00
dukovo-lab schedule save quick --preset quick --each-build
dukovo-lab schedule                  # each one: when it runs next, how it went last
dukovo-lab schedule start nightly    # run it now, as its time would
dukovo-lab schedule pause nightly    # and resume, remove
```

In a Dukovo checkout, `./ob lab schedule ...` does the same.

## What a scheduled run does

1. **Waits for its machines.** If a run is going on one of them, or on
   a machine of a pool it names (a spare is not waited for, as the run
   does not use it), it waits, for up to 12 hours, then starts.
2. **Brings the lab's own Dukovo checkout up to main.** This is a
   checkout of its own, `dukovo` in the store
   (`~/.local/share/dukovo-lab/dukovo`), cloned on the first start. It is
   reset to main on each start, so never work in it: whatever is typed
   there is lost. Build output stays, so later builds are quick.
3. **Builds the lab image and runs the tests,** with `./ob lab run` in
   that checkout: the image of that commit, with this lab's key, and the
   tests from that same commit. Runs are named after the build
   (`0.13.0 fd2a9e8`) and say which schedule started them. Needs you
   tests never run: nobody is there to do their steps.
4. **Sends or holds the findings.** A schedule that sends pushes the
   run's findings, and the run, to lab-results, as Send does
   ([Sending findings](sending.md)). If that fails (no clone, no
   network), the findings are held, and the schedule says why. A
   schedule that holds them leaves them on the page.
5. **Sends the notice,** when `notify.toml` is in place ([Notices](#notices));
   also when no round could start.
   A notice that cannot be sent is noted in the schedule's log and
   changes nothing else.

Scheduled runs take the lab's checkout one at a time: two schedules due
at once run one after the other. A schedule still going when its next
time comes skips that time.

## Notices

A scheduled run can tell your phone when it ends, with
[ntfy](https://ntfy.sh): a notice with the counts, the new failures and
the benched machines. It is sent when `notify.toml` is in the
configuration folder (`~/.config/dukovo-lab`):

```toml
ntfy = "https://ntfy.sh/dukovo-lab-7q4k9x2m5v8c1w3z"
link = "http://100.101.102.103:7420/?key=…#phone"
```

`ntfy` is the topic's address: ntfy.sh, or an ntfy server of your own.
`link` is optional: the address a tap on the notice opens, normally the
runner page's phone view on the lab PC's Tailscale address (see [On your
phone](runner-page.md#on-your-phone)). Its key is the share key, which
`dukovo-lab ui --share` keeps in `share_key` in the configuration folder,
so the link keeps working when `ui` restarts. If the link leaks, replace
the key with `ui --share <address> --new-share-key` and update the link. Without
`notify.toml` nothing is ever sent. The file holds the topic, which is
the only secret a topic has: `chmod 600` it, as the lab key is.

When a scheduled run ends, and when a run given `--notify` ends, the lab
sends one plain-text message to the topic:

```
Title:    Lab: 131 passed, 7 failing (3 new)
Priority: 4
Tags:     x,warning
Click:    <link>

New: php/suite on optiplex-1
New: php/suite on optiplex-2
Lab fault: storage/format on optiplex-3
Benched: optiplex-3
```

- The title has the run's passed count, and the failed and errored
  results as failing; the parenthesis counts the new ones: Dukovo's
  failures whose test was not red on that machine, or its pool, the
  time before (the page's New). A run that was stopped or that died says
  so.
- The body lists up to 5 new Dukovo failures and lab faults, as test and
  machine, then `and N more`, then the machines benched in that run.
  With nothing to list it says so.
- The priority is 4 (high) when something is new, a lab fault happened or
  a machine was benched, else 3. The tags show a cross or a check mark
  beside the title, and a warning for a lab fault or a benched machine.
- A schedule that cannot start a round at all (its checkout would not
  update or its image would not build) sends a notice titled `Lab: the
  <schedule> round did not start`, priority 4, whose body is the first
  line of why, without paths, addresses or URLs.
- Nothing else leaves the PC: no log, no result text (a reason can name
  an address), and no address other than the `link` you set.

The message is sent with `curl`, which the lab PC needs for this alone:
the lab's own HTTP client speaks plain HTTP, and ntfy.sh is HTTPS. The
topic, the link and the text go to curl on its standard input, not in
its arguments, so other accounts on the PC cannot see them in the process
list.

A notice that cannot be sent never fails the run or the schedule: the
reason goes to the schedule's log (`schedules/<name>.log` in the store),
or, for `run --notify`, to standard error and a detached run's
`output.log`. Check the setting, and that it reaches the phone, with

```sh
dukovo-lab notify --test
```

## The timer

A systemd timer on the lab PC runs `dukovo-lab schedule tick` each
minute. The tick starts each schedule whose time has come, or whose
branch has a new commit, then ends; each start runs in a process of its
own, so a long run never holds the timer up.

`./ob lab` puts the timer in place each time it runs. By hand:

```sh
dukovo-lab schedule install
```

It writes `dukovo-lab-schedules.service` and `.timer` to
`~/.config/systemd/user/`, turns the timer on, and turns on lingering
for your account (`loginctl enable-linger`), so schedules run whether you
are signed in or not. The service runs this same `dukovo-lab` with the
folders and PATH it was installed with; install again after moving
them. To turn schedules off for good: `systemctl --user disable --now
dukovo-lab-schedules.timer`.

- **Times** are in the zone of the schedules file, America/Denver unless
  it says otherwise, summer and winter alike: a 1:00 AM schedule runs at
  1:00 AM MST in winter and 1:00 AM MDT in summer. A time skipped when
  clocks go forward runs just after the jump; a time that happens twice
  when they go back runs once. The rules come from the PC's own time
  zone database (the `tzdata` package).
- **A time missed** while the lab PC was off or asleep runs when it
  wakes, if that is within 12 hours; an older one is skipped, and the
  schedule says it was missed.
- **Each new build** looks at main every 5 minutes. A new schedule, or
  one resumed, starts from the build after main as it is then.

## The schedules file

`schedules.json` in the configuration folder holds the schedules. The
commands and the page write it; it can be edited by hand too.

```json
{
  "zone": "America/Denver",
  "source": "https://github.com/dukovo-os/dukovo.git",
  "schedules": [
    {
      "name": "nightly",
      "title": "Full run",
      "preset": "full",
      "machines": ["qemu", "optiplex"],
      "at": "01:00",
      "send": true
    },
    {
      "name": "drivers",
      "tests": ["drivers"],
      "machines": ["optiplex"],
      "at": "02:00",
      "days": ["sun"],
      "send": false,
      "paused": true
    }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `zone` | The zone times are in. Default `America/Denver`. |
| `source` | Where the lab's own checkout is cloned from. Default Dukovo on GitHub. |
| `name` | Its name in commands: lowercase letters, digits and dashes. |
| `title` | What to call it on the page. Default: its preset's title. |
| `tests`, `preset`, `all` | What it runs: tests and categories, a preset, or every test. One of them. |
| `machines` | The machines it runs on, all at once. A pool is written `pool:<name>`: its machines share the tests out, as in [run](commands.md#run), instead of each running every test. |
| `at`, `days` | When: `HH:MM`, 24-hour, and the days (`mon` to `sun`; every day when left out). |
| `each_build` | `true` to run on each new commit on main instead of at a time. |
| `send` | `true` to send its findings when it ends, `false` to hold them. |
| `paused` | `true` while paused. |

Each schedule's state is in the store, under `schedules/`: when it last
ran and what came of it (`<name>.json`), and what its last start printed
(`<name>.log`), which is where to look when a build fails.
