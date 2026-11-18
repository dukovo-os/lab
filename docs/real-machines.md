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

A machine with neither AMT nor a serial port runs through the `plug`
driver instead: a Shelly smart plug turns it on and off, and its console
comes over the network. [A machine on a smart plug](#a-machine-on-a-smart-plug)
below sets one up; the rest of this page is about AMT machines, and what
it says of net boot, the lab network and the lab PC holds for both.

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
| `early-console` | The same cable: it carries the firmware's and GRUB's lines and the system's last lines before a power-off, which a `serial` step with `early = true` waits for. |
| `ssh` | The machine's own address on the lab network. |
| `peer` | The lab PC is `{peer}` (`10.77.0.1`) and the machine `{machine}` (`10.77.0.2`); the machine's ports are reached as they are. |
| `amt` | `{amt}` is AMT's own address (`10.77.0.10`), for a test that checks AMT keeps answering while the system runs. |
| `linux` | The machine is its own Linux twin: it boots the Linux kit from the network, then boots afresh to go on with the test. |
| `screen` | AMT's remote screen, a VNC server on port 5900: `screen` steps read its text and `screenshot` steps save it as a PNG. A failing test's screenshot comes from it too. |
| `keys` | The remote screen's keyboard, a US layout: `keys` and `type` steps. |
| `mouse` | The remote screen's mouse, which points at pixels of the screen as it is now: `point` and `click` steps. |

The first test that uses the screen, keyboard or mouse turns AMT's
remote screen on, with a password made for the run, and its session
stays open until the next power change. AMT plugs a keyboard and mouse
into the machine when a session opens, and turns power changes down
while one is open, so the driver closes its session before each power
change, and the next step that uses the screen opens another.

A person handles the network cable, USB sticks, cards and discs: Needs
you steps, with those controls set to `person` in the machine file.
Booting the install stick or DVD, and the attacker, are not done on
real machines yet; tests that need them are skipped there.

## The root grows to the disk's end

The image the lab writes has a 512 MiB root with about 285 MB free
after a boot, and leaves the rest of the disk empty. That is too little
for a PHP or PostgreSQL bundle or a website, which fill it and fail with
`ENOSPC`, so after each fresh boot, once the machine is ready and SSH
answers, and before the test's first step, the runner grows the root to
the disk's end with Dukovo's own `storage grow`, as root over SSH, as a
`root = true` step runs. It finds the root's disk from
`/system/live/spark/root`, and does nothing when the root already fills
the disk. The command and what it printed are in the driver's log
(`amt.log`, `plug.log`) and the test's log. When it fails, the fresh
boot fails with its output: a lab whose roots do not grow gives tests
false failures. It needs the lab's key and password (`dukovo-lab key`).

`grow_root = false` under `[amt]` or `[plug]` in the machine file turns
it off. QEMU does not grow: its disk is the image itself, with no room
after the root.

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
  each power change, each net boot, each request the writer made, each
  remote screen session, and what growing the root printed.
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
  gives up. A question AMT does not answer while the machine runs leaves
  the power as AMT last said it, or on after a power change: only AMT's
  word ends a wait as off. For a moment after it turns the machine on,
  AMT still says it is off, and a hard off asked while the machine
  starts is taken and not done: the driver waits until AMT says on, and
  asks a hard off again every 5 seconds while the machine stays on, for
  up to 30.
- AMT that stops answering altogether, no ping and no ARP reply for its
  address, while the lab network is up, has hung: only pulling the
  machine's power cord for 30 seconds brings it back. AMT 11.8 on an
  OptiPlex 7040 hung when asked for a hard off a second after turning
  the machine on, so the driver waits 10 seconds after turning the
  machine on or resetting it before the next power change, and
  `amt.log` notes each wait.
- AMT turns power changes down (`RequestPowerStateChange returned 2`)
  while a remote screen session is open. The driver closes its own
  first, but a VNC viewer a person has open on the machine blocks them,
  and the driver's own sessions too: close it before a run.
- When `amt.log` shows a net boot but no `GET kernel` after it, the
  machine did not load the writer: its firmware or the network GRUB went
  on to the disk, and `writer-<n>.log` shows the disk's GRUB menu. The
  net-boot server notes each request it turns away, without the token,
  and `journalctl -u dukovo-lab-dnsmasq` shows whether the machine asked
  for an address and the boot script.
- `dukovo-lab machines check <machine>` checks AMT, its remote screen,
  the serial port, the lab network, the TFTP folder, the network GRUB,
  the writer and the lab key, without changing anything.
- A screen that stays black over the remote screen while the machine
  runs needs a monitor or a display emulator plug on the board's Intel
  graphics.

## A machine on a smart plug

A real machine with no AMT and no serial port, such as a ThinkCentre M75q,
runs through the `plug` driver. What it needs:

- A firmware that boots from the network (UEFI IPv4 PXE) before its
  disk, every time, and powers the machine on after a power loss
  (After Power Loss set to Power On). The plug is its power switch: the
  driver switches the plug on, and the firmware does the rest.
- A Shelly plug (Gen 2 or later, such as the Plug US Gen 4) on the home
  network, with a fixed address and no password. The lab PC reaches it
  through its ordinary card, so the machine's own port is the only one on
  the lab network. The plug's `Switch.GetStatus` says whether its switch
  is on and what the machine draws.
- The lab network, as above, with the machine's address handed out by
  dnsmasq by its MAC address (`dhcp-host=<mac>,<machine_address>,<name>`):
  a plug machine's address is not the OptiPlex's `10.77.0.2`, so the
  machine file's `machine_address` names it, such as `10.77.0.3`.
- A disk the lab may overwrite, as above.
- The lab image's netlog: every lab image names the lab PC (`10.77.0.1`,
  port 6666) as netlog's target, and netlog sends the system's log over
  UDP from the machine's first moments, which the lab reads as the
  machine's console. Only the machine file's `machine_address` is listened
  to. An image that does not name the lab PC, or a machine whose network
  does not come up, leaves the console log empty.

Setting it up is the AMT machine's steps without MEBx and the serial
cable: the firmware settings above, then the plug machine's file
([machines/m75q.toml](../machines/m75q.toml)) with its MAC address, the
plug's address and the disk's serial number, then
`dukovo-lab machines check <machine>`.

The disk's serial number may be unknown: with `disk = "UNKNOWN"` the first
fresh boot fails with the writer's own message, which lists each disk it
found and its serial number (`no disk has the serial number UNKNOWN; the
disks are: nvme0n1 (...)`) in the error, `writer-<n>.log` and `plug.log`.
Put the NVMe's number in the machine file and run again; no disk is
written before that.

| Control | How |
| --- | --- |
| A fresh boot | The driver writes the boot script and power-cycles the plug (off, 5 seconds, on). The firmware boots from the network, the network GRUB reads the script, and the writer writes the image and checks it; once the hash matches, the machine restarts, finds no script, and boots its disk. The time this takes is added to the test's time limit. |
| `power = "on"` | The plug is switched on. When the plug is on but the machine draws less than `on_watts`, the machine shut itself down, and the plug is cycled. |
| `power = "reset"` | The plug off, 5 seconds, on. |
| `power = "cut"` (`cord`) | The plug off: a hard power loss, as when the cord is pulled. |
| `power = "button"` and `"off"` | Not available: a plug cannot ask the system to shut down, so these steps fail here, as on an AMT machine that does not answer; `machines check` says so. |
| `serial` | The netlog: each boot's console is `logs/<machine>/serial-<n>.log`, one line to a record, as a serial log reads. |
| `ssh`, `peer` | As for AMT: the machine's address and the lab PC's on the lab network. |
| `early-console`, `keys`, `mouse`, `screen`, `linux` | `none`: tests that need them are skipped. A `serial` step with `early = true` needs `early-console`: the netlog starts once the network is up and stops when it goes down, so it shows neither the firmware's and GRUB's lines nor the system's last lines before a power-off. |

The lab listens for one machine's netlog on one UDP port, so a run drives
one plug machine at a time. A second one needs its own `machine_address`,
and, at the same time, its own `netlog_port` and `boot_port`.

When something goes wrong, `logs/<machine>/plug.log` says what the driver
did: each switch of the plug, each net boot and each request the writer
made. A plug that stops answering fails the step with its address;
Wi-Fi drops a call now and then, so each call is tried three times.
`serial-<n>.log` stays empty while the machine runs when its network does
not come up or the image does not send its log to the lab PC; `plug.log`
and `writer-<n>.log` still show the fresh boot.
