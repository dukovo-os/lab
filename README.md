<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Dukovo Lab

The Dukovo Lab runs Dukovo's test suite on real machines and in QEMU,
then reports what fails. It boots each build, runs the tests a person or
a schedule picks, and turns every failure into a finding with its logs,
screenshots and the steps to repeat it.

This repository holds the harness, which runs on a Linux PC next to the
test machines and never on Dukovo itself. The tests live with the code
they test, in [dukovo-os/dukovo](https://github.com/dukovo-os/dukovo), so
a feature and its lab test land together and the lab always runs the
tests that match the build.

**Status:** in progress (item 14f of Dukovo's roadmap). `dukovo-lab`
runs tests on QEMU machines, writes results bundles, groups failures
into findings, sends them to the private results repository, runs
schedules on a systemd timer, and serves the runner page; real machines
come in the next pull requests.

## Getting started

In a Dukovo checkout, one command sets the lab up and opens the runner
page: it installs or updates `dukovo-lab`, makes the lab key and password, clones
lab-results for Send, puts the schedules' timer in place, builds the lab
image and opens the page.

```sh
./ob lab
```

By hand, on a Linux PC with Rust (through rustup), QEMU and OpenSSH:

```sh
sudo apt install qemu-system-x86 qemu-utils ovmf openssh-client git   # Linux Mint, Ubuntu
cargo install --locked --git https://github.com/dukovo-os/lab
dukovo-lab key          # once: the lab's SSH key and password, which lab images trust
git clone git@github.com:dukovo-os/lab-results.git ~/.local/share/dukovo-lab/lab-results   # once: for Send
```

Then, in a Dukovo checkout:

```sh
./ob lab list                       # the tests, by category
./ob lab run boot network           # build a lab image, run on QEMU
./ob lab run --all --machine qemu,qemu-bios
./ob lab ui                         # the runner page, in a browser on this PC
```

Each run leaves a results bundle under `~/.local/share/dukovo-lab/runs/`.

- [The runner page](docs/runner-page.md): `./ob lab ui`
- [Commands](docs/commands.md): every command, its JSON and exit statuses,
  results bundles, and troubleshooting
- [Sending findings](docs/sending.md): the lab-results repository, and
  what goes there
- [Schedules](docs/schedules.md): tests that run by themselves, at a time
  or on each new build on main
- [Test files](docs/test-files.md): writing a lab test
- [Machine files](docs/machine-files.md): adding a machine
- [Driving the lab from an agent](docs/agents.md)

## What it does and will do

- **Run tests singly, by category or on a schedule**, on one machine or
  several at once: boot and install, drivers, features, performance
  against Linux on the same machine, stress and soak, security, and the
  screen: keys typed, the pointer moved and clicked, and the console's
  text read from screenshots, from sign-in to the installer's screens.
- **Drive each machine through one interface**: QEMU first, then real
  machines through net boot and Intel AMT (power, boot choice,
  Serial-over-LAN and the remote screen). A test proven on QEMU runs
  unchanged on hardware.
- **Keep tests that need a person apart** (pulling a power cord, plugging
  in a USB stick), and skip them unless someone asks for them.
- **Show runs on a local page** that only the lab PC can reach, and do
  everything the page does from the command line too: every command
  takes `--json`, never prompts, and has documented exit codes, so
  scripts and agents can start, follow and report runs.
- **Send findings** to a private results repository, where each one gets
  a proposed fix that the maintainer approves.

## Rules

- The lab network never faces the internet, and an attack stays on a
  private network only the attacker and the machine under test share,
  reachable from nowhere else.
- A test's servers on the lab PC (a file share's far end, a Kerberos
  KDC) run in a folder of the test's own, on ports it picks, and end with
  the test; a test never changes the lab PC's own settings or services.
- Lab images carry a test agent that release images never contain.
- Security findings stay private until they are fixed.

## Contributing

Contributions follow Dukovo's
[CONTRIBUTING.md](https://github.com/dukovo-os/dukovo/blob/main/CONTRIBUTING.md):
every commit carries a `Signed-off-by` line under the
[Developer Certificate of Origin](DCO). Report security problems as
[SECURITY.md](SECURITY.md) says.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.
