<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Test files

Each lab test is one TOML file in Dukovo's repository, at
`lab/tests/<category>/<name>.toml`. The folder is the category and the
file name is the test's name, so `lab/tests/network/link-pull.toml` is the
test `network/link-pull`. A new file shows up in `dukovo-lab list` and
the runner page with no other change.

```toml
about = "The network cable comes out and goes back in, and the network comes back"
time = "3m"          # the longest the test may take, boot included
fresh = false        # true: boot afresh for this test alone

[[step]]
link = "down"
[[step]]
wait = "10s"
[[step]]
link = "up"
[[step]]
run = "ip link"
expect = "LOWER_UP"
expect_not = "NO-CARRIER"
```

## Keys

| Key | Required | Meaning |
| --- | --- | --- |
| `about` | yes | What the test checks, in one line. |
| `time` | yes | The longest it may take on KVM or real hardware: a whole number and `s`, `m` or `h`, at most `24h`. QEMU without KVM multiplies it by the machine's `tcg_slowdown`. |
| `fresh` | no | `true` boots the machine afresh, from a clean disk, for this test. Otherwise a test shares the boot the test before it left, when that test passed. |
| `[[step]]` | at least one | The steps, in order. |

## Categories

`categories.toml` beside the category folders names the categories, in
the order lists and the runner page show them, and the title the page
gives each. A category is a folder; a folder it does not name is an
error, so a test can't hide in a folder no list shows.

```toml
[[category]]
name = "network"                 # the folder: letters, digits, dashes
title = "Network"                # what lists and the page show, 40 characters at most
about = "Addresses, routes, names, ports, the firewall and the cable"
```

A suite with no `categories.toml` gets the built-in list: `boot`,
`drivers`, `features`, `performance`, `stress`, `security`, `console`,
`web` and `desktop`. Dukovo's suite names its own; add a category there
when a new kind of test needs one.

## Steps

Each step has exactly one action key, and only the options that go with it.

| Action | Options | Does | Control |
| --- | --- | --- | --- |
| `run = "<command>"` | `exit` (default 0), `expect`, `expect_not`, `root` | Runs the command over SSH as the `lab` account, or as root with `root = true` (see below). Passes when it exits with `exit` and its output (standard output and errors together) holds every `expect` text and no `expect_not` text. Each of those is a string or a list of strings. | ssh |
| `serial = "<text>"` | | Waits for the text on the serial console, after the last text a step found. A test that booted the machine searches its whole boot; a test sharing a boot sees only what came after it started; after `power = "on"` or `"reset"`, only what the new boot printed. | serial |
| `power = "on"` | | Powers the machine on. | power |
| `power = "off"` | | Presses the power button and waits until the machine is off. Fails if it is still on at the time limit. | power |
| `power = "button"` | | Presses the power button and goes on at once. | power |
| `power = "reset"` | | Presses reset. | power |
| `power = "cut"` | | Pulls the power cord: the system gets no warning. | cord |
| `link = "down"` or `"up"` | | Pulls or plugs in the network cable. | link |
| `usb = "plug"` | `image` or `blank` | Plugs in a USB stick: a copy of `image`, a file in the test's folder, or with `blank = "64M"` an empty stick of that size, from `1M` to `4G` (`K`, `M` or `G`). | usb |
| `usb = "unplug"` | | Pulls the stick out. | usb |
| `keys = ["ctrl-alt-delete"]` | | Presses each combination in turn: QEMU key names joined by `-`. | keys |
| `screenshot = "<name>"` | | Keeps the screen in the bundle as `screenshots/<machine>/<category>-<test>-<name>.png`. | screen |
| `wait = "ready"` | | Waits until the machine has booted, its ready line anywhere in the boot going now (since the last `power = "on"` or `"reset"`, even if a `serial` step found it), and, when it has an SSH channel, SSH answers. Use it after `power = "on"` or `"reset"`, and after a restart from inside, as `reboot`, with a `serial` step for the new boot's ready line first. | |
| `wait = "off"` | | Waits until the machine turns itself off, as after `poweroff`. Fails if it is still on at the time limit. | power |
| `wait = "<time>"` | | Waits that long. | |

Every test also needs `power` and `serial`, for the boot it starts with.

## Running as root

Root never signs in over SSH, and elevate always asks for a password, so
a step with `root = true` runs its command as `elevate -S <command>` and
gives elevate the lab password on its standard input (ADR 0406 in
Dukovo's repository). `dukovo-lab key` makes the password beside the lab
key, `~/.config/dukovo-lab/lab_password`, and `ob image --lab` puts its
hash in the image. The password never leaves the lab PC: anything a
command prints has it replaced by `********` before it is shown or kept.
Without the file, a run that picks a root step stops before it starts
and says how to make one.

Only the first command is elevated: in `run = "a | b"` only `a` runs as
root. To run a pipeline, redirection or several commands as root, hand
them to a shell: `run = "sh -c 'mkfs-fat /devices/sdb && fsck-fat -n /devices/sdb'"`.

```toml
[[step]]
run = "service --status-all"
root = true
expect = "[ + ]  sshd"
```

## How a test runs

1. If the test is `fresh`, the test before it failed, or the machine is
   off, the machine boots from a clean disk holding the image under test
   and the runner waits until it is ready: the machine's ready line (by
   default `Boot complete.`) on the serial console, then SSH answering.
   That boot counts as the test's first step.
2. The steps run in order. The first that fails ends the test; when the
   machine can take screenshots, the screen is kept as
   `<category>-<test>-at-failure.png`.
3. The test passes when every step passed within its time limit.

A test that needs a control the machine does not have is skipped there.
A test whose controls include one a person does on that machine is a
Needs you test there. Runs skip those unless they are given
`--with-person`, or the test is ticked in the runner page's Needs you
group; then each step a person does waits for them to say it is done.
See [machine files](machine-files.md).

## Presets

`presets.toml` beside the category folders names sets of tests to run
together. The runner page offers them, with how long each took last time;
`dukovo-lab run --preset <name>` runs one.

```toml
[[preset]]
name = "quick"                 # what --preset takes: letters, digits, dashes
title = "Quick check"          # what the page shows
about = "A few minutes: boots, runs commands, pulls the cable"
tests = ["boot/reaches-ready", "ssh/commands", "network/link-pull"]   # tests and categories
```

A preset naming a test or category that does not exist stops every
command, as a broken test file does.

## Writing a good lab test

- Check what a person would see: a command's output, a line on the
  console, a screen.
- Keep each test to one thing, so its name says what broke.
- Give `time` some room: it is a limit for a hang, not a benchmark.
- Run as root only the steps that need it, so the rest check what an
  ordinary account sees.
- Find a USB stick by its size (`lsblk -dn`), never by a fixed device
  name, before formatting it: the system disk's name differs from
  machine to machine.
- To hand something from a `lab` step to a root step, put it in the lab
  account's home, `/users/lab`: each account on Dukovo has a `/tmp` of
  its own.
- Prefer sharing a boot. Use `fresh = true` only when the test changes
  the machine in a way later tests should not inherit, or checks the boot
  itself.
