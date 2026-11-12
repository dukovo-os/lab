<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Real machines

The lab runs the same tests on real machines as on QEMU, through the
`amt` driver: Intel AMT turns the machine on and off, picks its next
boot, and shows its screen and works its keyboard and mouse; a net boot
writes the image under test to its disk before each fresh boot, a serial
cable brings its console to the lab PC, and the lab network carries SSH
and the lab PC's side of a test.

## What a machine needs

- Intel AMT (a vPro machine), with its network port on the lab network.
- A serial port (on the board, or a card Dukovo drives), cabled to a
  serial port of the lab PC's, such as a USB serial cable.
- A disk the lab may overwrite: every fresh boot writes the image over
  it.
- For tests that use the screen, keyboard or mouse: the picture on the
  board's own Intel graphics, the only one AMT's remote screen sees. Some
  machines draw it only while a monitor, or a display emulator plug, is
  on one of the board's ports.

## The lab network

The lab network is its own: a second network card in the lab PC and a
switch, with no route to the home network or the internet. Its
addresses:

| Address | What |
| --- | --- |
| `10.77.0.1/24` | The lab PC's lab card. No gateway and no DNS server. |
| `10.77.0.2` | The machine, handed out by dnsmasq by its MAC address. |
| `10.77.0.10` | AMT, set in MEBx. AMT shares the machine's network port, and serves its remote screen on port 5900. |

## Setting the machine up

Once, at the machine:

1. Update its firmware: the updates carry fixes for the management
   engine AMT runs in.
2. In the firmware's setup: SATA in AHCI mode, Secure Boot off, the
   UEFI network stack and PXE on, deep sleep control off (AMT must be
   reachable while the machine is off), and power on after a power loss.
3. In MEBx (Ctrl+P as the machine starts): change the password from
   the default, turn AMT on, give it the static address `10.77.0.10/24`,
   set Admin Control Mode, set User Opt-in (user consent) to none, and
   turn Serial-over-LAN, storage redirection and KVM Feature Selection
   on. The driver turns on the remote screen's port 5900 itself.
4. Note the network card's MAC address and the disk's serial number
   (`lsblk -o NAME,SERIAL` from a Linux live USB) for the machine file.

## Setting the lab PC up

Once, on the lab PC, in this order: `./ob lab` reads the machine files
and needs the lab card's address.

1. Give the lab card the lab address, with no gateway, naming the card
   (`ip link` lists them):

   ```sh
   nmcli connection add type ethernet ifname <card> con-name dukovo-lab \
       ipv4.method manual ipv4.addresses 10.77.0.1/24 ipv4.never-default yes \
       ipv6.method disabled
   ```

2. Make sure dnsmasq is there: `sudo apt install dnsmasq-base`.
   NetworkManager brings it on Mint, Ubuntu and Debian; the `dnsmasq`
   package's own service is not needed.
3. Let the lab's user read the serial port, then sign in again:
   `sudo usermod -aG dialout $USER`.
4. Copy [machines/optiplex.toml](../machines/optiplex.toml) to
   `~/.config/dukovo-lab/machines/`, and put in the MAC address and the
   disk's serial number.
5. Put AMT's password in a file of its own beside it, which only you can
   read:

   ```sh
   install -m 600 /dev/null ~/.config/dukovo-lab/machines/optiplex-amt-password
   ${EDITOR:-nano} ~/.config/dukovo-lab/machines/optiplex-amt-password
   ```

6. `./ob lab` in a Dukovo checkout sets up dnsmasq and the TFTP folder
   on the lab card, and installs the network GRUB and the writer (see
   [Net boot](net-boot.md)).
7. Check it all: `dukovo-lab machines check optiplex`. Each line is
   `ok`, a `note`, or a `PROBLEM` that stops runs there, and says how to
   put it right; it exits 3 while there is a problem.

## What the driver does

| Control | How |
| --- | --- |
| A fresh boot | AMT turns the machine off, then boots it from the network once into the writer, which writes the image to the disk and reads it back. Once the hash matches, the machine restarts into its disk. The time this takes is added to the test's time limit. |
| `power = "on"` | AMT turns the machine on. |
| `power = "reset"` | AMT's reset. |
| `power = "cut"` (`cord`) | AMT's hard off: the system gets no warning, as when the cord is pulled. |
| `power = "button"` and `"off"` | AMT asks the system to shut down, as the power button does. AMT does this only when the system answers it, which Dukovo does not yet, so these steps fail here; `machines check` says so. |
| `serial` | The serial cable: each boot's console is `logs/<machine>/serial-<n>.log`. |
| `ssh` | The machine's own address on the lab network. |
| `peer` | The lab PC is `{peer}` (`10.77.0.1`) and the machine `{machine}` (`10.77.0.2`); the machine's ports are reached as they are. |
| `linux` | The machine is its own Linux twin: it boots the Linux kit from the network, then boots afresh to go on with the test. |
| `screen` | AMT's remote screen, a VNC server on port 5900: `screen` steps read its text and `screenshot` steps save it as a PNG. A failing test's screenshot comes from it too. |
| `keys` | The remote screen's keyboard, a US layout: `keys` and `type` steps. |
| `mouse` | The remote screen's mouse, which points at pixels of the screen as it is now: `point` and `click` steps. |

The first test that uses the screen, keyboard or mouse turns AMT's
remote screen on, with a password made for the run, and its session
stays open for the rest of the run. AMT plugs a keyboard and mouse into
the machine when a session opens, so the session staying open keeps
them there across boots, as a person's would be.

A person handles the network cable, USB sticks, cards and discs: Needs
you steps, with those controls set to `person` in the machine file.
Booting the install stick or DVD, and the attacker, are not done on
real machines yet; tests that need them are skipped there.

## Safety

- AMT's password stays in its file: it is never written to a log, a
  bundle or output, and the lab refuses a password file others can read.
- The remote screen's password is made afresh for each run and is never
  written anywhere. VNC signs in with DES, which is weak, so the remote
  screen, like AMT, is reached on the lab network alone.
- AMT, dnsmasq and the driver's net-boot server are reached on the lab
  network alone.
- Only the disk with the machine file's serial number is written, and
  only by the writer, which finds it by that number.

## When something goes wrong

- `logs/<machine>/amt.log` in the run's bundle says what the driver did:
  each power change, each net boot, each request the writer made, and
  each remote screen session.
- `logs/<machine>/writer-<n>.log` is the writer's console for boot `n`.
  When it and `serial-<n>.log` stay empty while `amt.log` shows the
  writer at work, the machine file's `console` names another port than
  the one cabled: with Serial-over-LAN on, AMT's own port is `ttyS4`,
  and a serial card's ports come after it, from `ttyS5`. When they hold
  mostly zero bytes, the port runs at another speed than the machine
  file's `baud`. The lab puts the machine's `console` alone on the
  writer's and the twin's command lines, in place of the kit's own,
  because Linux keeps the speed of a port's first `console=`.
- AMT refuses connections for a moment when the machine has been off
  and AMT left alone for a while: its engine dozes, and the first
  connection wakes it. The driver keeps trying for 20 seconds before it
  gives up.
- `dukovo-lab machines check <machine>` checks AMT, its remote screen,
  the serial port, the lab network, the TFTP folder, the network GRUB,
  the writer and the lab key, without changing anything.
- A screen that stays black over the remote screen while the machine
  runs needs a monitor or a display emulator plug on the board's Intel
  graphics.
