<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Test files

Each lab test is one TOML file in Dukovo's repository, at
`lab/tests/<category>/<name>.toml`. The folder is the category and the
file name is the test's name, so `lab/tests/drivers/link-pull.toml` is the
test `drivers/link-pull`. A new file shows up in `dukovo-lab list` and
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

The folders, in the order lists show them: `boot` (boot and install),
`drivers` (drivers and hardware), `features`, `performance`, `stress`
(stress and soak), `security`, `console` (console UI), `web` (web console)
and `desktop`. Any other folder under `lab/tests/` is an error.

## Steps

Each step has exactly one action key, and only the options that go with it.

| Action | Options | Does | Control |
| --- | --- | --- | --- |
| `run = "<command>"` | `exit` (default 0), `expect`, `expect_not` | Runs the command over SSH as the `lab` account. Passes when it exits with `exit` and its output (standard output and errors together) holds every `expect` text and no `expect_not` text. Each of those is a string or a list of strings. | ssh |
| `serial = "<text>"` | | Waits for the text on the serial console, after the last text a step found. A test that booted the machine searches its whole boot; a test sharing a boot sees only what came after it started. | serial |
| `power = "on"` | | Powers the machine on. | power |
| `power = "off"` | | Presses the power button and waits until the machine is off. Fails if it is still on at the time limit. | power |
| `power = "button"` | | Presses the power button and goes on at once. | power |
| `power = "reset"` | | Presses reset. | power |
| `power = "cut"` | | Pulls the power cord: the system gets no warning. | cord |
| `link = "down"` or `"up"` | | Pulls or plugs in the network cable. | link |
| `usb = "plug"` | `image` | Plugs in a USB stick holding a copy of `image`, a file in the test's folder. | usb |
| `usb = "unplug"` | | Pulls the stick out. | usb |
| `keys = ["ctrl-alt-delete"]` | | Presses each combination in turn: QEMU key names joined by `-`. | keys |
| `screenshot = "<name>"` | | Keeps the screen in the bundle as `screenshots/<machine>/<category>-<test>-<name>.png`. | screen |
| `wait = "ready"` | | Waits until the machine has booted and, when it has an SSH channel, SSH answers. Use it after `power = "on"` or `"reset"`. | |
| `wait = "<time>"` | | Waits that long. | |

Every test also needs `power` and `serial`, for the boot it starts with.

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
Needs you test there; runs skip those until the runner page can ask a
person to act (lab PR 2). See [machine files](machine-files.md).

## Writing a good lab test

- Check what a person would see: a command's output, a line on the
  console, a screen.
- Keep each test to one thing, so its name says what broke.
- Give `time` some room: it is a limit for a hang, not a benchmark.
- Prefer sharing a boot. Use `fresh = true` only when the test changes
  the machine in a way later tests should not inherit, or checks the boot
  itself.
