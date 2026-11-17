# iztop

A terminal ZFS pool I/O monitor — `zpool iostat -v` drawn in the style of
[igtop](https://github.com/GladTechInfo/igtop) and [istop](https://github.com/GladTechInfo/istop):
pastel gradient bars coloured cell by cell, rounded boxes, damped motion. Written entirely in Rust,
with no `unsafe` code.

## What it shows

* **One box per pool**, one row per vdev and disk, laid out as `zpool iostat -v` lays them out.
  The box title carries the pool's health, a capacity gauge, fragmentation, and scrub or resilver
  progress with rate and time left while one runs.
* **Busy** — a gradient bar per disk: the share of time the disk had I/O in flight, from
  `/proc/diskstats`, read from the whole disk under a partition. A mirror, raidz or pool row shows
  its busiest disk, since one saturated disk holds up the rest.
* **Operations, bandwidth and latency** (total wait) per row — `zpool iostat`'s own figures over the
  refresh interval.
* **Faults on the row** — a vdev or disk that is not ONLINE, or has READ/WRITE/CKSUM errors, is
  named in red with its counters.
* **Bandwidth graph** — ten minutes of read and write for one pool, istop's mirrored network graph.
  By default it appears only when every pool fits and there is height to spare, and follows the
  busiest pool.

Columns are dropped from the right as the terminal narrows — latency first, then operations — so
it fits a half-width pane. Long by-id disk names keep the start of the model and the whole serial.

`iztop --once` prints one interval as a plain-text table for scripts.

## Keys

| Key | Action |
|---|---|
| `q` `Esc` | quit |
| `↑ ↓ PgUp PgDn Home End` | scroll through the pools when they do not all fit |
| `d` | pools menu: order them, show or hide them; `W` there saves the choice |
| `l` | hide or show the disks inside mirrors and raidz vdevs |
| `g` | graph: when there is room, always, never |
| `n` `b`, `a` | graph the next or previous pool; back to the busiest |
| `p` | full device names or short ones |
| `+` `-` | slower or faster refresh (250 ms to 10 s) |
| `Space` | pause |
| `s`, `i` | damped or raw bars; icons or ASCII |
| `?` | help |
| `Ctrl+Z` | suspend |

## Configuration

`~/.config/iztop/iztop.conf` (or `-c FILE`) holds the pool order and the hidden pools, and is written
only when you press `W` in the pools menu. Pools not listed follow by name, so a newly imported pool
still appears. Pools named on the command line (`iztop tank`) are shown even if hidden.

## Installing

Each [release](https://github.com/GladTechInfo/iztop/releases/latest) carries ready-made builds:

| File | For | Installs to |
|---|---|---|
| `iztop_<version>-1_amd64.deb` | Debian, Ubuntu, Proxmox VE | `/usr/bin/iztop` |
| `iztop-<version>-1.x86_64.rpm` | Fedora, RHEL, Rocky, Alma, openSUSE | `/usr/bin/iztop` |
| `iztop-<version>-x86_64-linux` | Any x86-64 Linux, including Alpine | wherever you put it |
| `SHA256SUMS` | Checksums of the three above | |

All three contain the same static binary and need no libraries — only the `zpool` command, from
OpenZFS (2.3 or later for health and scrub detail). Before a release's files are uploaded, each
package is installed, run and removed in containers of Debian 12 and 13, Ubuntu 22.04 and 24.04,
Fedora, Rocky Linux 9, openSUSE Tumbleweed and Alpine.

```sh
# the latest release; or set ver=X.Y.Z for a particular one
ver=$(curl -s https://api.github.com/repos/GladTechInfo/iztop/releases/latest | sed -n 's/.*"tag_name": "v\(.*\)".*/\1/p')
base=https://github.com/GladTechInfo/iztop/releases/download/v$ver
curl -LO "$base/SHA256SUMS"
curl -LO "$base/iztop_${ver}-1_amd64.deb"      # or the .rpm, or the binary
sha256sum --check --ignore-missing SHA256SUMS  # must print "OK"

sudo apt install ./iztop_*_amd64.deb                            # Debian, Ubuntu, Proxmox
sudo dnf install ./iztop-*.x86_64.rpm                           # Fedora, RHEL and rebuilds
sudo zypper install --allow-unsigned-rpm ./iztop-*.x86_64.rpm   # openSUSE
sudo install -m 0755 iztop-*-x86_64-linux /usr/local/bin/iztop  # anything else
```

The packages are not signed; `SHA256SUMS` is the check that a download is the one published here.

## Where the figures come from

* A long-lived `zpool iostat -p -v -l -P -y -T u <interval>`: operations, bandwidth and latency.
  Per-vdev statistics reach user space only as a packed nvlist through an ioctl on `/dev/zfs`,
  whose layout is private to OpenZFS; the command's output is its stable, documented face.
* `zpool status -j` (OpenZFS 2.3 or later) and `zpool list`, every 5 seconds: health, errors,
  fragmentation, scrub and resilver. On older OpenZFS the JSON is skipped and health comes from
  `zpool list` alone.
* `/proc/diskstats`: busy.

Both `zpool` feeds run on their own threads, so a slow or hung pool never stalls the screen.

## Building

```sh
cargo build --release           # for this machine
./build.sh                      # static musl binary for any x86-64 Linux, AVX-512 checked
```

Needs a Nerd Font for the glyphs (`--no-icons` otherwise). Run as root, or as a user allowed to
run `zpool iostat`.

## Licence

GPL-3.0-or-later.
