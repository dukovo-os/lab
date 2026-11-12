<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Net boot

A real machine starts each test from a fresh copy of the image under
test, written to its disk by a small program booted from the lab
network: the writer. The same path boots the machine's Linux twin. This
page is the contract between `dukovo-lab` and the parts `ob lab`
installs on the lab PC: dnsmasq's setup, the network GRUB and the
writer. [Real machines](real-machines.md) sets a machine up.

## What `ob lab` installs

| Part | Where | What it does |
| --- | --- | --- |
| dnsmasq | the lab card only | DHCP on the lab network, each machine's fixed address by its MAC, no gateway and no DNS server offered; TFTP from the TFTP folder; the boot file `dukovo-lab/grubx64.efi` for x86-64 UEFI clients. It never answers on any other card. |
| TFTP folder | `/srv/dukovo-lab/tftp` | Readable by dnsmasq. `dukovo-lab/boot/` in it is writable by the lab PC's user, for boot scripts. |
| Network GRUB | `dukovo-lab/grubx64.efi` in the TFTP folder | GRUB built for the network, with `http` and `tftp`, whose built-in script reads the machine's boot script and otherwise leaves for the next boot entry, the disk (below). |
| The writer's kit | `/srv/dukovo-lab/writer` | `kit.toml`, in the format of the Linux kit (`--linux`): `kernel`, `initrd`, `cmdline` and `done`, with the kernel and initrd beside it. |

The network GRUB's built-in script:

```
source (tftp)/dukovo-lab/boot/${net_default_mac}.cfg
exit
```

A machine with no boot script, booted from the network by its own boot
order or by hand, so goes on to its disk.

## One boot

1. The driver writes the machine's boot script,
   `dukovo-lab/boot/<mac>.cfg` in the TFTP folder (the MAC in lower
   case, with colons). It loads a kernel and initrd from the driver's
   HTTP server, under a token new to this boot:

   ```
   set timeout=0
   insmod http
   echo 'dukovo-lab: loading the writer'
   linux (http,10.77.0.1:7431)/<token>/kernel <command line>
   initrd (http,10.77.0.1:7431)/<token>/initrd
   boot
   ```

   The server listens on four ports, from the machine file's
   `boot_port`, and each boot's URLs name the next one in turn. GRUB
   opens its connections from the same source port, 21550 and up, with
   nearly the same sequence number, on every boot, and the lab PC
   remembers a connection for a minute after it closes. A boot that came
   within that minute on the same port would be turned away, and GRUB,
   answering the lab PC's replies, keeps the old connection remembered
   for as long as it tries.

2. AMT is asked to boot the machine from the network once, and to turn
   it on. The firmware asks dnsmasq for an address and its boot file,
   GRUB reads the script, and the kernel boots.
3. The script is removed as the boot ends, before the writer restarts
   the machine, so the boot after goes on to the disk.

The writer's command line is its kit's, then:

| Word | What |
| --- | --- |
| `console=` | The machine file's `amt.console`. |
| `dukovo.lab=` | The boot's base URL, such as `http://10.77.0.1:7431/<token>/`. |
| `dukovo.lab.ip=` | The machine's address on the lab network, with its prefix: `amt.machine_address` and `/24`, such as `10.77.0.2/24`. |
| `dukovo.lab.mac=` | The MAC address of the card to give it, `amt.mac` in lower case. |

The writer gives that card that address, with no gateway, and reaches
the base URL's host directly. The Linux twin's command line is its
kit's, then `console=`. What either prints on its console goes to the
bundle: `logs/<machine>/writer-<n>.log` for the writer,
`linux-boot-<n>.log` for the twin.

## The writer's protocol

Every path is under the base URL, and is 404 once the boot is over.

| Request | Answer |
| --- | --- |
| `GET plan` | The plan, as JSON (below). |
| `GET data` | The image's data: each extent's bytes, in the plan's order, back to back. |
| `POST log` | A line of progress, kept in the driver's `amt.log`. Optional. |
| `POST done` | `{"sha256": "<hex>"}`: the SHA-256 of the disk's first `size` bytes, read back from the disk. 200 and `restart` when it is the image's; 409 when it is not. |
| `POST failed` | Why, as text. 200. |

The plan:

```json
{"version": 1, "disk": "S3Z5NB0K123456A", "size": 608174080,
 "sha256": "<hex>", "extents": [[0, 1048576], [4194304, 65536]]}
```

`size` is the image's size in bytes, `sha256` the hash of all of it,
and `extents` where it holds data, as offset and length in bytes, in
order and never touching. Everything else in the first `size` bytes is
zeros.

The writer:

1. Fails unless `version` is 1.
2. Finds the disk whose serial number is `disk`. If there is none, or
   more than one, it fails without writing anything. No other disk is
   ever opened for writing.
3. Fails unless the disk holds `size` bytes and a mebibyte more.
4. Writes each extent at its offset as the data streams in, and zeroes
   the rest of the first `size` bytes. Then it zeroes the disk's last
   mebibyte, so no partition table an earlier test left there is found.
5. Flushes the disk's write cache, reads the first `size` bytes back
   past the page cache, and hashes them.
6. Posts `done`. On 200 it restarts the machine, into its disk. On any
   other answer, or any failure (after posting `failed`), it powers the
   machine off. It never restarts into the network.

It prints what it is doing on its console as it goes.

The image is written as it is: its GPT's backup header stays at the end
of the image, not of the disk, as when a smaller disk's image is copied
to a larger one.

## Safety

- Nothing the lab serves listens beyond the lab card: dnsmasq is bound
  to it, and the driver's HTTP server to the lab PC's address on it.
- A boot's URLs carry a token new to that boot, so a machine still in
  an earlier boot cannot fetch the plan or post an outcome for the boot
  going now.
- The driver checks what the writer read back against the image's own
  hash before the machine boots from its disk; a test never runs on a
  disk that is not the image.
