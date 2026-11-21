<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Real machines

The lab runs the same tests on real machines as on QEMU, through the
`amt` driver: Intel AMT turns the machine on and off, picks its next
boot, and shows its screen and works its keyboard and mouse; a serial
cable brings its console to the lab PC; a net boot writes the image
under test to its disk before each fresh boot, and the lab network
carries SSH and the lab PC's side of a test.

A machine without AMT runs through the `plug` driver instead: a Shelly
smart plug turns it on and off, and its console comes over the network.
Cables to the lab PC can add its screen, keyboard and mouse, and a console
from power-on. [A machine on a smart plug](#a-machine-on-a-smart-plug)
below sets one up; the rest of this page is about AMT machines, and what
it says of net boot, the lab network and the lab PC holds for both.

## What a machine needs

- Intel AMT (a vPro machine), with its network port on the lab network.
- A serial cable from a port of the machine's to one of the lab PC's,
  for its console. AMT's Serial-over-LAN can carry the console with no
  cable, but the disk writer's download stalls while a session is open
  (see [The serial console](#the-serial-console)), so a machine with a
  serial port keeps its cable.
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
   on. The driver turns on the remote screen's port 5900 itself, and
   Serial-over-LAN's listener if it is off.
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
3. Let the lab's user read the serial cable's port, then sign in
   again: `sudo usermod -aG dialout $USER`.
4. Copy [machines/optiplex.toml](../machines/optiplex.toml) to
   `~/.config/dukovo-lab/machines/`, and put in the MAC address, the
   disk's serial number and the serial cable's port.
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

### The lab PC's programs

Some tests run the far side of what they check on the lab PC: an NFS
server, a KDC, a mail catcher, WireGuard. A test whose program the lab PC
lacks is skipped, saying so. `sudo sh scripts/lab-pc-tools.sh` installs
them all on a Debian, Ubuntu or Mint lab PC; `dukovo-lab machines check`
ends with a line that says `ok` when every program is there, or names
each missing program and how many tests need it.

## What the driver does

| Control | How |
| --- | --- |
| A fresh boot | AMT turns the machine off, then boots it from the network once into the writer, which writes the image to the disk and reads it back. Once the hash matches, the machine restarts into its disk. The time this takes is added to the test's time limit. |
| `power = "on"` | AMT turns the machine on. |
| `power = "reset"` | AMT's reset. |
| `power = "cut"` (`cord`) | AMT's hard off: the system gets no warning, as when the cord is pulled. |
| `power = "button"` and `"off"` | AMT asks the system to shut down, as the power button does. AMT does this only when the system answers it, which Dukovo does not yet, so these steps fail here; `machines check` says so. |
| `serial` | AMT's Serial-over-LAN, or the serial cable the machine file names: each boot's console is `logs/<machine>/serial-<n>.log`. |
| `early-console` | The same: it carries the firmware's and GRUB's lines and the system's last lines before a power-off, which a `serial` step with `early = true` waits for. |
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

### The serial console

A machine file that names a `serial` port has its console read from
that cable, and Serial-over-LAN is left as it is. That is how the lab's
machines run: on the lab's OptiPlex 7040 (AMT 11.8, an I219-LM network
port), the disk writer's download stalled at 40 to 50% of the image in
every boot of the first run with Serial-over-LAN open, and never
finished, where the same write over the cable takes 14 seconds. As its
network driver starts, the writer's Linux says `Reset blocked by ME`:
while a Serial-over-LAN session is open, AMT keeps the network port's
PHY from being reset. Whether that is the cause is not known yet. A
machine file with no `serial` port reads the console over
Serial-over-LAN, as follows: for a machine with no serial port of its
own, once the stall is solved.

AMT's management engine shows the system a serial port of its own, at
PCI 00:16.3 on Intel boards, and hands what the system writes there to
one console on the network at a time, on AMT's port 16994. With no
console there, AMT drops the text. So the driver turns Serial-over-LAN
on as it starts, opens a session, signs in with AMT's user and password
(by HTTP digest: the password never goes over the network), and keeps
the session open until the run ends, across every boot and power
change, opening another at once if one ends. Linux names the port
`ttyS4` and Dukovo does too, and Dukovo writes its log to every serial
port, AMT's among them, from the kernel's first lines on. GRUB shows
its menu there too (`pci,00:16.3`), as the install stick's menu does.

The session is AMT's one: while the lab has it, a person's terminal
(MeshCommander, `amtterm`) cannot open another, and while a person's
is open, the run cannot start. AMT turns power changes down while a
remote screen session is open, and AMT 11.8 while a Serial-over-LAN
session is open too: the driver then closes its session, asks again,
and opens it once the change is made, and `amt.log` says so.

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

`grow_root = false` under `[amt]`, `[plug]` or `[qemu]` in the machine
file turns it off. A QEMU machine grows its root the same way: each boot's
disk is an 8 GiB overlay over the image, with the room after the root
that the image itself lacks.

## The clock is set to the lab PC's

A real machine keeps its real-time clock from one test to the next, and
the lab network reaches no time server. A test that sets the clock years
ahead and fails before putting it back would leave every test after it
a clock past the expiry of the update server's signatures and of the
lab's certificates. So after each fresh boot, right after the root
grows, the runner sets the machine's clock to the lab PC's, as root, with
`timedatectl set-time`; a clock that cannot be set fails the boot. QEMU
machines start each boot from the lab PC's clock, and are left alone.

## Browsers open the machine by `localhost`

A passkey is bound to a name, and several certificate checks need the
machine to be reached by the name it knows itself by, `localhost`. A
QEMU machine's ports are on the lab PC's own loopback, so a `browser`
step opens `https://127.0.0.1:PORT`. A real machine is reached by its
address on the lab network, which it does not know itself by, so before
the first `browser` step of a test the runner starts an `ssh -N -L
127.0.0.1:FREE:127.0.0.1:PORT` to the machine, as the lab account, with
the lab key and the SSH options of every other step (and the boot's
known-hosts file), and the step's page is `https://127.0.0.1:FREE`. The
machine sees each connection come from itself. The forward is one of the
test's lab PC processes: it ends with the test, and the test's log says
so. The machine's `sshd` must allow TCP forwarding, as the lab image's
does.

Only what a browser opens changes. `host`, `run` and `attack` steps, and
`{machine}` and `{machine-port:N}` in them, keep the machine's address.
When the forward does not start (the key is refused, the machine is
off, the port is taken), the browser is given the machine's address as
before, the test's log says why, and the test's script decides whether
it can go on: a script that needs a name skips itself. The forward's
own output is `logs/<machine>/<test>-forward-<port>.log` in the bundle.

## The health check

A machine with a bad setup fails every test it is given: a root too
small, a clock years off. Each such failure costs a test's whole time and
would be filed as a failure of Dukovo's. So once per fresh boot, after the
root has grown and the clock is set, and before the test's first step, the
runner asks the machine one question over SSH, as the lab account, which
takes about a second. It checks:

- The free space on the root: at least 1 GiB. The check is left out when
  the machine file has `grow_root = false`, as the lab image's own root is
  small by design.
- The clock: within 5 seconds of the lab PC's, either way.

A check that fails is an `error` for that test, not a failure: it says
nothing about Dukovo. The reason names the check and what to do, as
`health check failed after the fresh boot: clock 41 s off the lab PC's,
over the 5 s allowed. ...`. What the check found is in the test's log, as
`Health check after boot 2: root 5859 MiB free; clock 0 s off the lab
PC's: ok`, and in the driver's log. A part of the answer that cannot be
read (no `df` on the machine, say) is left out and the log says so.

A machine whose fresh boots fail the check twice in a row is benched for
the rest of the run: its remaining tests are skipped, each with a reason
such as `optiplex-1 was benched: its health check failed twice (clock 41 s
off the lab PC's, over the 5 s allowed)`. The other machines carry on. A
fresh boot that passes starts the count again. `run --resume` does not pick
up tests skipped this way: run them again once the machine is put right.
Only AMT and plug machines are checked; a QEMU machine starts from a clean
disk each time.

The console is not checked: the runner waits for the machine's ready line
on the console before the check, so a boot that reaches it has shown its
console lines already. A console that shows nothing fails the boot.

## Several identical machines

Identical machines share a round: three OptiPlex 7040s that share one test
list finish in about a third of the time one machine takes. Each machine has
a file of its own, and they name one pool:

```toml
pool = "optiplex"
```

To add `optiplex-2`, `optiplex-3` and `optiplex-4`, copy `optiplex-1.toml`
(the example in the repository is [machines/optiplex.toml](../machines/optiplex.toml))
to `optiplex-2.toml` and so on, and change what belongs to the machine
alone:

- `[amt]` `address`: the machine's AMT address, set in its MEBx, and a
  `password_file` of its own if its AMT password differs.
- `mac`: its network card's address, which dnsmasq also hands the
  machine's address by (`dhcp-host=<mac>,<machine_address>,<name>`), so
  give each machine its own `machine_address` as well.
- `disk`: the serial number of its disk, from `lsblk -o NAME,SERIAL`.
- `serial`: its own serial cable's port, best by its
  `/dev/serial/by-id/` name, as USB serial ports are renumbered when
  cables move.
- `boot_port`: the first of its four net-boot ports. Machines in one run
  need ports of their own, so each machine's is four more than the one
  before.

Keep `pool = "optiplex"` and the `[controls]` the same in every file: a
pool whose machines differ in a control is refused when the machine files
are read, naming the machine and the control. Then check each machine
(`dukovo-lab machines check optiplex-2`) and run on the pool:

```sh
dukovo-lab run --all --machine pool:optiplex --image ...
```

A machine named on its own, such as `optiplex-1`, still runs every test.
Mark a machine `spare = true` to keep it for reruns and bisects: a run on
the pool leaves it out, and it is named directly when wanted. Plug
machines in a pool each need their own `machine_address`, `boot_port` and
Shelly plug (see [A machine on a smart plug](#a-machine-on-a-smart-plug)).

If one machine of a pool will not start, or its health check benches it,
the others take its share of the queue and the run says which machine left
and why (a `machine-out` event). See [run](commands.md#run).

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
  writer at work, look in `amt.log` for the Serial-over-LAN session:
  each one opened, each that ended and why, and why one would not
  open. With the session open, the machine file's `console` names
  another port than the one the lab reads: AMT's own is `ttyS4`, and a
  serial card's ports come after it, from `ttyS5`; a cable on the
  board's COM1 is `ttyS0`. Dukovo says `Serial: the Intel AMT
  Serial-over-LAN port at ... is the terminal /devices/ttyS4` when it
  finds AMT's port. On a cable, logs holding mostly zero bytes mean the
  port runs at another speed than the machine file's `baud`. The lab
  puts the machine's `console` alone on the writer's and the twin's
  command lines, in place of the kit's own, because Linux keeps the
  speed of a port's first `console=`.
- A Serial-over-LAN session that will not open says why in the run's
  first lines: AMT turns it down while another console, such as a
  person's terminal, holds it; a wrong password is tried again only
  once a minute, as AMT locks a user out after several.
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
  Serial-over-LAN (a session opens and closes) or the serial port, the
  lab network, the TFTP folder, the network GRUB, the writer and the lab
  key, without changing anything.
- A screen that stays black over the remote screen while the machine
  runs needs a monitor or a display emulator plug on the board's Intel
  graphics.

## A machine on a smart plug

A real machine without AMT, such as a ThinkCentre M75q or an OptiPlex 7040
with AMT turned off, runs through the `plug` driver. With no cables it has
a console over the network and nothing else; [the cables](#the-cables-of-a-plug-machine)
below add a screen, a keyboard and mouse, and a console from power-on.
What it needs:

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

Setting it up is the AMT machine's steps without MEBx, and without the
cables until [they are added](#the-cables-of-a-plug-machine): the
firmware settings above, then the plug machine's file
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
| `serial` | The netlog, or the serial cable when the file names one: each boot's console is `logs/<machine>/serial-<n>.log`. Over netlog it is one line to a record, as a serial log reads. |
| `ssh`, `peer` | As for AMT: the machine's address and the lab PC's on the lab network. |
| `early-console`, `linux` | The serial cable, when the file names one (`plug.serial`); otherwise `none`. A `serial` step with `early = true` needs `early-console`: the netlog starts once the network is up and stops when it goes down, so it shows neither the firmware's and GRUB's lines nor the system's last lines before a power-off. |
| `screen` | The HDMI capture stick, when the file names one (`plug.capture`); otherwise `none`. |
| `keys`, `mouse` | The CH9329 cable, when the file names one (`plug.hid`); otherwise `none`. |

Several plug machines run at once. They share the netlog port, and the lab
sorts each datagram by the machine's `machine_address`, so each machine
needs a `machine_address` of its own (two machines with the same one in a
run are refused), its own `boot_port`, and its own Shelly plug.

### The cables of a plug machine

A plug machine may have up to three cables to the lab PC. Each is optional,
and each turns on the controls in the table above; without it those
controls stay `none` and tests that need them are skipped. The cables are
new and have not yet passed a run on real hardware. The example file
[machines/optiplex-plug.toml](../machines/optiplex-plug.toml) names them
for OptiPlex 2 to 4, but no pool depends on them yet.

What to buy:

- A UVC HDMI capture stick that offers YUYV at the screen's size, such as
  an MS2130 stick, on a USB 3 port of the lab PC. The HDMI cable goes from
  the board's own video port to the stick, with no graphics card fitted.
- A CH9329 USB-serial-to-HID cable. Its serial end goes into the lab PC
  and its USB end into the machine, which sees a keyboard and an absolute
  mouse.
- An FTDI serial cable from the lab PC to the board's COM1 header or port.

The lab PC needs `v4l2-ctl`, which takes the stick's pictures, and an
account that may open the devices:

```sh
sudo apt install v4l-utils
sudo usermod -aG video,dialout "$USER"
```

Log in again afterwards, so the groups take effect.

USB devices are renumbered when cables move, and cheap cables of one model
share one `/dev/.../by-id/` name. Find each device's `by-path` name, which
follows the USB port it is plugged into, and put those in the machine file:

```sh
ls -l /dev/v4l/by-path/ /dev/serial/by-path/
```

The stick appears under `/dev/v4l/by-path/` (the name ending in
`video-index0` is the picture), the two serial cables under
`/dev/serial/by-path/`. Plug each into the same port again later, or
update the file. `plug.serial` and `plug.hid` must name different ports.

`plug.capture_size` must be the size of the machine's screen, which
Dukovo prints in its console as `Framebuffer: WxH, ...`. The default is
`1920x1080`. A picture of another size is refused with a message that
names the setting to change, because a scaled picture has no pixel of the
machine's own and no text can be read from it. The text reader allows each
pixel to stray a little in brightness, as a capture stick's colour
conversion rounds.

Run `dukovo-lab machines check <machine>` to see each cable answer:
`serial port` opens the serial cable, `capture` takes a picture and says
how much of it is lit, and `keyboard and mouse` asks the CH9329 cable
whether the machine has set it up. While the machine is off, some of
these are notes and not faults, because the CH9329 and the stick may draw
their power from the machine; check again with it on.

### One pool with the AMT OptiPlex

A pool needs identical controls. Once a hardware run passes on the
cables, OptiPlex 1 (the `amt` driver) and OptiPlex 2 to 4 (the `plug`
driver, with all three cables) can share the pool `optiplex`. OptiPlex 1's
file then sets `amt = "none"` under `[controls]`, since the plug machines
have no AMT. The test that needs AMT then runs from a second file for
OptiPlex 1 with no pool and AMT left on, such as `optiplex-1-amt.toml`,
named on its own and never in the same run as the pool, since both files
drive the one machine. Until then OptiPlex 2 to 4 have no `pool` line.

When something goes wrong, `logs/<machine>/plug.log` says what the driver
did: each switch of the plug, each net boot and each request the writer
made. A plug that stops answering fails the step with its address;
Wi-Fi drops a call now and then, so each call is tried three times.
`serial-<n>.log` stays empty while the machine runs when its network does
not come up or the image does not send its log to the lab PC; `plug.log`
and `writer-<n>.log` still show the fresh boot.
