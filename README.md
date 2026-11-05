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

**Status:** planned. Work starts after Dukovo's Phases 13 and 14 (item
14f of the roadmap). Nothing here runs yet.

## What it will do

- **Run tests singly, by category or on a schedule**, on one machine or
  several at once: boot and install, drivers, features, performance
  against Linux on the same machine, stress and soak, security, and the
  screen.
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

- The lab network never faces the internet, and attacks stay on it.
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
