#!/bin/sh
# SPDX-License-Identifier: MIT
#
# TASK-111 spike: what a real Hyper-V guest answers about its display stack.
#
# Runs inside a VMLord Ubuntu VM. Nothing here changes the VM in a way that
# matters to VMLord except `desktop`, which installs GNOME and switches the
# machine to the graphical target -- run it on a VM you are willing to throw
# away.
#
# Stages, in order:
#
#   sudo sh probe.sh stock     # a cloud image as VMLord builds it, no desktop
#   sudo sh probe.sh desktop   # install GNOME + GDM, then reboot
#   sudo sh probe.sh greeter   # after the reboot, sitting at the GDM greeter,
#                              # WITHOUT logging in
#   sudo sh probe.sh collect   # tar up everything for the report
#
# Each stage appends to /var/log/vmlord-drm-spike/<stage>.log and prints the
# same text, so an SSH session that dies loses nothing.
set -u

DIR=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-/var/log/vmlord-drm-spike}
STAGE=${1:-help}

[ "$(id -u)" = 0 ] || { echo "run as root: sudo sh $0 $STAGE" >&2; exit 1; }
mkdir -p "$OUT"

LOG="$OUT/$STAGE.log"

say()  { printf '\n=== %s\n' "$*" | tee -a "$LOG"; }
note() { printf '%s\n' "$*" | tee -a "$LOG"; }

# Echo a command, then its output and exit status. A failing command is data,
# not an abort: "the guest cannot do this" is half of what the spike asks.
run() {
	printf '\n$ %s\n' "$*" | tee -a "$LOG"
	sh -c "$*" 2>&1 | tee -a "$LOG"
	printf '[exit %s]\n' "$?" | tee -a "$LOG"
}

need_packages() {
	say "installing probe tools: $*"
	run "apt-get update -qq"
	run "DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $*"
}


# The driver bound to the first real card, from the kernel's own uevent.
driver_of_card1() {
	for card in /sys/class/drm/card*; do
		case "$card" in *-*) continue;; esac
		[ -e "$card/device/uevent" ] || continue
		sed -n 's/^DRIVER=//p' "$card/device/uevent" | head -n 1
		return
	done
}

# Everything about one card that decides whether a session can use it.
describe_card() {
	node=$1
	name=$(basename "$node")
	note ""
	note "--- $name"
	run "drm_info $node 2>&1 | head -n 24"
	run "readlink -f /sys/class/drm/$name/device 2>/dev/null || echo '(no device link -- faux bus?)'"
	run "cat /sys/class/drm/$name/device/uevent 2>/dev/null || echo '(no uevent)'"
	run "udevadm info --query=property --name=$node | grep -E 'TAGS|ID_SEAT|ID_PATH|DEVPATH' || true"
	run "drm_info $node 2>&1 | grep -iE 'writeback|\"type\"|Formats|Framebuffer size|Width|Height' | head -n 30"
}

# ---------------------------------------------------------------------------
# Facts every stage wants: what the kernel is, what it will let us load, and
# what DRM devices exist right now.
# ---------------------------------------------------------------------------
inventory() {
	say "kernel and image"
	run "uname -a"
	run "cat /etc/os-release"
	run "cat /proc/cmdline"
	run "mokutil --sb-state || true"
	run "cat /sys/kernel/security/lockdown || true"

	say "modules relevant to display"
	run "lsmod | grep -Ei 'drm|hyperv|video|fb' || echo 'none loaded'"
	run "modinfo hyperv_drm | head -n 12 || echo 'hyperv_drm: no module (built in, or absent)'"
	run "modinfo vkms       | head -n 12 || echo 'vkms: not shipped in this kernel package set'"
	run "modinfo simpledrm  | head -n 6  || echo 'simpledrm: built into the kernel, not a module'"
	run "grep -E 'CONFIG_DRM_(HYPERV|VKMS|SIMPLEDRM)' /boot/config-\$(uname -r) || true"

	say "DRM devices as they are"
	run "ls -l /dev/dri/ 2>&1"
	for card in /sys/class/drm/card*; do
		[ -e "$card" ] || continue
		case "$card" in *-*) continue;; esac   # skip connector entries
		name=$(basename "$card")
		note ""
		note "--- $name"
		run "readlink -f $card/device/driver"
		run "readlink -f $card/device"
		run "cat $card/device/uevent 2>/dev/null"
	done

	say "connectors, their status and the modes they offer"
	for conn in /sys/class/drm/card*-*; do
		[ -e "$conn" ] || continue
		note ""
		note "--- $(basename "$conn")"
		run "cat $conn/status $conn/enabled 2>/dev/null"
		run "cat $conn/modes 2>/dev/null || echo '(no modes listed)'"
	done

	# Whether logind will hand a DRM device to a graphical session at all.
	# A card udev has not tagged master-of-seat is invisible to GDM no
	# matter how correct its KMS implementation is.
	say "udev seat tagging -- what logind will let a session open"
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		run "udevadm info --query=property --name=$node | grep -E 'ID_PATH|ID_SEAT|TAGS|DEVPATH' || true"
	done
	run "loginctl seat-status seat0 2>&1 | head -n 40"

	say "what the kernel said about display at boot"
	run "dmesg | grep -Ei 'drm|framebuffer|hyperv_video|simple-framebuffer|vkms' | head -n 60"
}

# ---------------------------------------------------------------------------
# Stage: stock
# ---------------------------------------------------------------------------
stage_stock() {
	need_packages "drm-info libdrm-tests build-essential libdrm-dev pkg-config"

	inventory

	say "full DRM capability dump (planes, formats, properties, writeback)"
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		run "drm_info $node 2>&1 | head -n 400"
	done

	# Nothing holds DRM master before a display manager exists, so this is
	# the one moment we can ask the driver to actually set a mode. The
	# answer bounds what a desktop can ask for later.
	say "mode setting on the stock driver -- what resolutions are accepted"
	drv=$(driver_of_card1)
	note "driver under test: ${drv:-none}"
	run "modetest -M ${drv:-none} -c 2>&1 | head -n 60"
	# The first connected connector: what a compositor would pick too.
	conn=$(modetest -M "${drv:-none}" -c 2>/dev/null |
	       awk '$1 ~ /^[0-9]+$/ && $3 == "connected" {print $1; exit}')
	note "connected connector: ${conn:-none found}"
	for mode in 1024x768 1920x1080 2560x1440; do
		run "timeout 6 modetest -M ${drv:-none} -s ${conn:-0}:$mode -v 2>&1 | tail -n 20"
	done

	say "framebuffer budget the synthetic video device was given"
	run "dmesg | grep -Ei 'hyperv_drm|hyperv_video|mmio|vram' | head -n 30"
	run "cat /proc/iomem | grep -i -A2 -B2 'hyperv\|framebuffer' || true"

	# The in-tree alternative to a private module. If it loads, registers a
	# card, gets tagged for seat0 and exposes a writeback connector, VMLord
	# needs no kernel module of its own at all.
	say "in-tree vkms as a candidate backend"
	run "modprobe vkms 2>&1"
	run "lsmod | grep vkms || echo 'vkms did not load'"
	run "ls -l /dev/dri/"
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		describe_card "$node"
	done
	run "modprobe -r vkms 2>&1 || true"

	# Which Mesa a compositor will load, and whether VMLord's GPU payload
	# has put its own in front of the distribution's. A guest whose GBM
	# comes from the WSL Mesa is not the guest Ubuntu ships.
	say "userspace graphics stack in this guest"
	run "ls -l /opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu 2>/dev/null | head -n 20 || echo 'no VMLord Mesa staged'"
	run "cat /etc/ld.so.conf.d/*vmlord* /etc/ld.so.conf.d/*mesa* 2>/dev/null || echo 'no ld.so.conf entry'"
	run "ldconfig -p | grep -iE 'libgbm|libEGL_mesa|libGLX_mesa' || true"
	run "ls -l /dev/dxg 2>/dev/null || echo 'no /dev/dxg -- no GPU-PV in this VM'"

	say "what a DKMS module would cost this guest"
	run "apt-get install -s -y dkms linux-headers-\$(uname -r) 2>&1 | tail -n 12"
	run "ls -d /usr/src/linux-headers-* 2>/dev/null || echo 'no headers installed'"

	note ""
	note "stage 'stock' done -- log at $LOG"
}

# ---------------------------------------------------------------------------
# Stage: desktop
# ---------------------------------------------------------------------------
stage_desktop() {
	say "installing GNOME and GDM"
	run "apt-get update -qq"
	run "DEBIAN_FRONTEND=noninteractive apt-get install -y ubuntu-desktop-minimal"
	run "systemctl set-default graphical.target"
	run "systemctl status gdm --no-pager 2>&1 | head -n 20"

	note ""
	note "rebooting into the graphical target."
	note "Do NOT log in when it comes back -- the greeter is what stage"
	note "'greeter' looks at. Reconnect over SSH and run:"
	note ""
	note "    sudo sh $DIR/probe.sh greeter"
	run "sleep 3; systemctl reboot"
}

# ---------------------------------------------------------------------------
# Stage: greeter
# ---------------------------------------------------------------------------
stage_greeter() {
	say "is there a graphical session before anyone logged in"
	run "systemctl is-active gdm graphical.target"
	run "loginctl list-sessions"
	run "loginctl show-session \$(loginctl list-sessions --no-legend | awk 'NR==1{print \$1}') -p Type -p Class -p Active -p Seat 2>&1"
	run "ps -eo pid,user,comm | grep -Ei 'gdm|gnome-shell|mutter|Xwayland' || echo 'no compositor running'"

	inventory

	say "which DRM device the greeter actually opened"
	run "ls -l /proc/*/fd 2>/dev/null | grep -c dri || true"
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		run "fuser -v $node 2>&1"
	done

	say "what the compositor said while starting"
	run "journalctl -b --no-pager -u gdm 2>&1 | tail -n 60"
	run "journalctl -b --no-pager -t gnome-shell 2>&1 | tail -n 60"
	run "journalctl -b --no-pager | grep -iE 'mutter|no.*drm|kms|udev.*card|MetaRenderer' | tail -n 60"

	say "how many monitors the desktop believes it has"
	run "drm_info 2>&1 | grep -iE 'Connector|status|Object|modes' | head -n 60"

	say "reading the greeter's framebuffer out of the live compositor"
	if [ ! -x "$DIR/plane_capture" ]; then
		run "cc -O2 -Wall -o $DIR/plane_capture $DIR/plane_capture.c \$(pkg-config --cflags --libs libdrm)"
	fi
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		run "$DIR/plane_capture $node 60 $OUT/greeter-\$(basename $node).ppm"
	done
	run "ls -l $OUT/*.ppm 2>/dev/null || echo 'nothing captured'"

	note ""
	note "stage 'greeter' done -- log at $LOG"
	note "If a .ppm was written, it is the GDM greeter as a capture backend"
	note "would have seen it. Keep it: it is the proof, not the log."
}


# ---------------------------------------------------------------------------
# Stage: pattern
#
# The control experiment. A blank capture has two possible causes and the
# logs cannot tell them apart: either nothing can be read out of this driver,
# or nothing was ever drawn into it. So take the compositor out of the
# picture and let modetest draw a test pattern of its own, then read that.
# Bars in the PPM mean the capture path is sound and the blank frame was the
# compositor's doing.
# ---------------------------------------------------------------------------
stage_pattern() {
	drv=$(driver_of_card1)
	say "stopping the display manager so modetest can take DRM master"
	run "systemctl stop gdm"
	run "sleep 2"

	conn=$(modetest -M "${drv:-none}" -c 2>/dev/null |
	       awk '$1 ~ /^[0-9]+$/ && $3 == "connected" {print $1; exit}')
	note "driver ${drv:-none}, connector ${conn:-none found}"

	say "painting a test pattern and reading it back"
	modetest -M "${drv:-none}" -s "${conn:-0}" -v >"$OUT/pattern-modetest.log" 2>&1 &
	pattern_pid=$!
	sleep 3

	if [ ! -x "$DIR/plane_capture" ]; then
		run "cc -O2 -Wall -o $DIR/plane_capture $DIR/plane_capture.c \$(pkg-config --cflags --libs libdrm)"
	fi
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		run "$DIR/plane_capture $node 60 $OUT/pattern-\$(basename $node).ppm"
	done

	kill "$pattern_pid" 2>/dev/null
	run "tail -n 20 $OUT/pattern-modetest.log"
	run "systemctl start gdm"
	note ""
	note "stage 'pattern' done -- log at $LOG"
}

# ---------------------------------------------------------------------------
# Stage: collect
# ---------------------------------------------------------------------------
stage_collect() {
	tar="/tmp/vmlord-drm-spike-$(hostname)-$(date +%Y%m%d-%H%M%S).tar.gz"
	tar -C "$(dirname "$OUT")" -czf "$tar" "$(basename "$OUT")"
	echo "$tar"
	echo "Copy it off the guest, e.g. from the host:  scp <vm>:$tar ."
}

case "$STAGE" in
	stock)   stage_stock;;
	desktop) stage_desktop;;
	greeter) stage_greeter;;
	pattern) stage_pattern;;
	collect) stage_collect;;
	*)
		cat <<'EOF'
usage: sudo sh probe.sh <stage>

  stock     what a VMLord cloud VM's display stack is before any desktop
  desktop   install GNOME + GDM and reboot (destructive to this VM)
  greeter   at the GDM greeter, not logged in: what the compositor bound to
            and whether its framebuffer can be read from outside it
  pattern   with GDM stopped, modetest paints a test pattern and the probe
            reads it back -- separates a broken capture from a blank desktop
  collect   tar up /var/log/vmlord-drm-spike for the report
EOF
		exit 2;;
esac
