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
| `boot` | no | `"image"` (the default) boots the image under test. `"stick"` boots the install stick with a blank disk: see [Installer tests](#installer-tests). |
| `disk` | no | With `boot = "stick"`, the blank disk's size: `8G` by default, from `1G` to `2048G`. |
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
| `bench = "<command>"` | `rows` (required), `limit` (default 1.00), `root` | Runs a benchmark program over SSH and holds each of its `rows` to the same row on the machine's Linux twin: passes when Dukovo takes at most `limit` times Linux's time per operation (see [Performance tests](#performance-tests)). | ssh, linux |
| `attack = "<command>"` | `exit` (default 0), `expect`, `expect_not`, `root` | Runs the command on the attacker, a second machine on a private network only it and the machine under test share, and holds its output as `run` does. `{target}` in the command, `expect` or `expect_not` becomes the machine's address on that network. See [Attack tests](#attack-tests-the-security-range). | attack |
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
| `type = "<text>"` | | Types the text on a US keyboard: `\n` is Enter, and `{password}` types the lab password, which is never shown or kept. A character with no key on a US keyboard is an error when the suite loads. | keys |
| `point = [x, y]` | | Moves the pointer to that pixel of the screen, counted from its top left corner. An error if the screen is smaller. | mouse |
| `click = "left"` | | Presses a pointer button where the pointer is and lets it go: `left`, `right` or `middle`. | mouse |
| `screen = "<text>"` | | Waits until the screen shows the text, a string or a list of strings that must all show, each within one row (see [Reading the screen](#reading-the-screen)). | screen |
| `screen_gone = "<text>"` | | Waits until the screen no longer shows the text. | screen |
| `screenshot = "<name>"` | | Keeps the screen in the bundle as `screenshots/<machine>/<category>-<test>-<name>.png`, and, when the run has the console's fonts, its text beside it as `.txt`. | screen |
| `wait = "ready"` | | Waits until the machine has booted, its ready line anywhere in the boot going now (since the last `power = "on"` or `"reset"`, even if a `serial` step found it), and, when it has an SSH channel, SSH answers. Use it after `power = "on"` or `"reset"`, and after a restart from inside, as `reboot`, with a `serial` step for the new boot's ready line first. | |
| `wait = "off"` | | Waits until the machine turns itself off, as after `poweroff`. Fails if it is still on at the time limit. | power |
| `wait = "<time>"` | | Waits that long. | |

Every test also needs `power` and `serial`, for the boot it starts with.

## Running as root

Root never signs in over SSH, and elevate always asks for a password, so
a `run` or `bench` step with `root = true` runs its command as `elevate -S <command>` and
gives elevate the lab password on its standard input (ADR 0408 in
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

## Reading the screen

`screen` and `screen_gone` read the console's text from a screenshot,
with no OCR: Dukovo's console says on the serial console where its cells
are (`consoled: console of 160x50 cells with font 8x16 at (0, 0)`), and
each cell's lit pixels are looked up in the console's own fonts, which
`ob lab` passes with `--fonts`. A cell no font draws, such as the
cursor, reads as `?`. A run without fonts skips tests that read the
screen and says how to get them.

A step polls the screen twice a second until the text shows, or is gone,
or the test's time runs out. A failing step shows what the screen held
then. Text stays on the screen as it scrolls up, so wait for something
new: to wait for a command's output and not the command line that typed
it, split a label in two in the command, as
`type = "echo \"in\"\": $(whoami)\"\n"` then `screen = "in: lab"`.

```toml
[[step]]
screen = "Name:"
[[step]]
type = "lab\n"
[[step]]
screen = "Password:"
[[step]]
type = "{password}\n"
[[step]]
screen = "lab@dukovo ~ $"
```

A test that types `{password}` needs the lab password, as a root step
does (see [Running as root](#running-as-root)).

## Installer tests

A test with `boot = "stick"` starts from the install stick, which `ob
lab` builds and passes with `--stick`: the machine boots with the stick
plugged in and first in the boot order, and a blank disk of `disk`'s
size. The runner waits for nothing, as an installer has no SSH; the
test's steps drive the installer's screens and check them. `usb =
"unplug"` pulls the stick, after which the machine boots from the disk.
The test after it boots afresh from the image under test, as the disk no
longer holds it. A run without a stick skips these tests and says why.

```toml
about = "The installer erases a blank disk and installs Dukovo"
time = "30m"
boot = "stick"

[[step]]
screen = ["Step 1 of 5", "How do you want to install?"]
[[step]]
keys = ["ret"]
```

## Performance tests

A `bench` step measures Dukovo against Linux on the same machine. Its
command is a benchmark program that prints lines in the format of
Dukovo's benchmark programs and `ob bench`:

```text
BENCH name=<row> ns=<nanoseconds per operation> iterations=<count>
```

Linux's numbers come from the machine's Linux twin: a Linux kernel and
an initramfs (the Linux kit) booted on a machine set up the same way,
whose first program runs the same benchmarks built for Linux, prints the
same lines on the serial console, and then says it is done. `ob lab`
builds the kit from the Linux kernel `ob bench` holds Dukovo to (ADR
0290 in Dukovo's repository) and passes `--linux`, so the lab and CI
measure against the same Linux and the same benchmark code.

```toml
[[step]]
bench = "/system/tests/posix/bench"
rows = ["posix_fork_wait", "posix_fork_exec_wait"]
```

- Each row passes when Dukovo's time is at most `limit` times Linux's:
  1.00 by default, parity, the bar every row of `ob bench` is held to.
  `limit` goes from 0.1 to 10, for a row with a known gap that is being
  worked on. A row printed more than once keeps its fastest.
- The twin boots once a run on each machine, the first time a bench step
  runs there, and its numbers serve every bench step after. The machine
  under test is paused while the twin runs, so the two never share the
  host.
- One machine measures at a time, so machines running side by side do
  not slow each other's numbers.
- A failing step names the rows over their limit and shows the table of
  both systems' times; the Linux boot's console is kept in the bundle as
  `logs/<machine>/linux-boot-<n>.log`.
- Numbers from an emulator mean little, so on QEMU without KVM a bench
  step still runs and keeps both tables, then is skipped rather than
  passed or failed.
- A run without a kit (no `--linux`) skips tests with bench steps and
  says why. Machines with no Linux twin (`linux = "none"`) skip them too.

## Attack tests (the security range)

A test with an `attack` step probes the machine under test from the
outside, the way someone on the same network would. For it the runner
sets up a security range: a private network that only the machine under
test and an attacker share, reachable from nowhere else — not the lab's
own network, not the internet. The attacker is a second machine the
driver stands up for the test; under QEMU it is a second Dukovo guest
joined to the machine by a private virtual network, with no way out of
its own.

```toml
about = "The machine answers SSH on its port and nothing else"
time = "5m"
fresh = true

[[step]]
attack = "nc -z -w2 {target} 22"
[[step]]
attack = "nc -z -w2 {target} 23"
exit = 1
```

- `{target}` becomes the machine under test's address on the private
  network (`10.9.9.1`); the attacker's is `10.9.9.2`. A test uses
  `{target}` rather than a fixed address, so the same test reads on any
  machine. It is replaced in the command and in `expect` and
  `expect_not`.
- An `attack` step runs its command on the attacker and passes on the
  same terms as a `run` step: the exit status, and the text its output
  must and must not hold. It runs over SSH as the attacker's `lab`
  account, or as root with `root = true`.
- The first `attack` step of a boot brings the attacker up and gives each
  side its address on the private network. That setup runs as root, so an
  attack test needs the lab password, as a root step does (see [Running
  as root](#running-as-root)).
- The private network port is there only on a boot armed for an attack,
  so an idle port never shows up in a network test: an attack test always
  boots afresh, and its boot is never shared with the test after it.
- A machine with no attacker (`attack = "none"` in its file) skips these
  tests and says so. The attacker is the machine's own, never a person's.

Attack tests live under `lab/tests/security/`. Keep them to probing the
machine from the outside over the private range; a check of the machine's
own state belongs in a `run` step.

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
- A test that signs in at the console signs out at its end, so the next
  test finds the sign-in as a fresh boot leaves it.
