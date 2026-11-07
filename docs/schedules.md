<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Schedules

A schedule runs tests by itself, with the runner page closed. It runs
at a time of day in the lab's zone (Mountain Time), on every day or on
the days it names, or on each new build on Dukovo's main branch. It runs
a preset, tests and categories, or every test, on one or more machines
at once. When it ends, it sends its findings to lab-results, or holds
them for you to send from the page.

Make them on the runner page (Schedules, New schedule) or with
commands:

```sh
dukovo-lab schedule save nightly --preset full --machine qemu,optiplex --at 01:00 --title "Full run"
dukovo-lab schedule save drivers drivers --machine optiplex --at 02:00 --days sun --hold
dukovo-lab schedule save quick --preset quick --each-build
dukovo-lab schedule                  # each one: when it runs next, how it went last
dukovo-lab schedule start nightly    # run it now, as its time would
dukovo-lab schedule pause nightly    # and resume, remove
```

In a Dukovo checkout, `./ob lab schedule ...` does the same.

## What a scheduled run does

1. **Waits for its machines.** If a run is going on one of them, it
   waits, for up to 12 hours, then starts.
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

Scheduled runs take the lab's checkout one at a time: two schedules due
at once run one after the other. A schedule still going when its next
time comes skips that time.

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
| `machines` | The machines it runs on, all at once. |
| `at`, `days` | When: `HH:MM`, 24-hour, and the days (`mon` to `sun`; every day when left out). |
| `each_build` | `true` to run on each new commit on main instead of at a time. |
| `send` | `true` to send its findings when it ends, `false` to hold them. |
| `paused` | `true` while paused. |

Each schedule's state is in the store, under `schedules/`: when it last
ran and what came of it (`<name>.json`), and what its last start printed
(`<name>.log`), which is where to look when a build fails.
