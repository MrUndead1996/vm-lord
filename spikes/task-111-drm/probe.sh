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
# modetest -M wants the DRM driver's own name -- the string drm_info prints
# as "Driver:", the one drmGetVersion returns. The bus driver in sysfs is a
# different name for the same device (simple-framebuffer carries simpledrm,
# and no "simple-framebuffer" module exists for modetest to open), so ask the
# card, not the bus.
driver_of_card1() {
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		drm_info "$node" 2>/dev/null |
			sed -n 's/.*Driver: \([A-Za-z0-9_-]*\).*/\1/p' | head -n 1
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
	# Two traps live in this one command. "modetest -s" holds the mode until
	# its stdin closes, and "-v" replaces that wait with an endless vsynced
	# page-flip loop -- on a driver whose flips never complete it sits in the
	# ioctl, ignores a plain SIGTERM, and never closes the pipe a "| tail"
	# would be waiting on. So: no -v, stdin already at EOF, output to a file
	# that is read after the process is gone, and a SIGKILL behind the
	# timeout for the case where the ioctl does wedge.
	for mode in 1024x768 1920x1080 2560x1440; do
		run "timeout -k 2 6 modetest -M ${drv:-none} -s ${conn:-0}:$mode </dev/null >$OUT/modeset-$mode.log 2>&1"
		run "tail -n 20 $OUT/modeset-$mode.log"
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
# Stage: extra
#
# The 24.04 cloud image has neither hyperv_drm nor vkms on disk, even though
# its kernel config builds both as modules: linux-image-virtual pulls only
# linux-modules, and every DRM driver beyond the builtin simpledrm lives in
# linux-modules-extra. So "use the stock driver" is not free -- it is a
# package, and this stage prices it and then asks what the two candidates
# actually look like once they are there.
# ---------------------------------------------------------------------------
stage_extra() {
	say "which DRM drivers this image ships at all"
	run "ls /lib/modules/\$(uname -r)/kernel/drivers/gpu/drm/ 2>&1 | head -n 30"
	run "apt-get install -s -y linux-modules-extra-\$(uname -r) 2>&1 | tail -n 8"

	say "installing the module package the stock drivers live in"
	run "DEBIAN_FRONTEND=noninteractive apt-get install -y linux-modules-extra-\$(uname -r)"

	for mod in hyperv_drm vkms; do
		say "$mod as a candidate backend"
		run "modinfo $mod | head -n 8 || echo '$mod still absent'"
		run "modprobe $mod 2>&1"
		run "lsmod | grep $mod || echo '$mod did not load'"
		run "dmesg | tail -n 15"
		run "ls -l /dev/dri/"
		for node in /dev/dri/card*; do
			[ -e "$node" ] || continue
			describe_card "$node"
		done
	done

	say "mode setting on whatever now owns the display"
	drv=$(driver_of_card1)
	note "driver under test: ${drv:-none}"
	run "modetest -M ${drv:-none} -c 2>&1 | head -n 60"
	conn=$(modetest -M "${drv:-none}" -c 2>/dev/null |
	       awk '$1 ~ /^[0-9]+$/ && $3 == "connected" {print $1; exit}')
	note "connected connector: ${conn:-none found}"
	for mode in 1024x768 1920x1080 2560x1440; do
		run "timeout -k 2 6 modetest -M ${drv:-none} -s ${conn:-0}:$mode </dev/null >$OUT/extra-modeset-$mode.log 2>&1"
		run "tail -n 20 $OUT/extra-modeset-$mode.log"
	done

	note ""
	note "stage 'extra' done -- log at $LOG"
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
	# Here the pattern has to stay on screen while the capture runs, so
	# modetest must not exit -- but "-v" would wedge it in a page-flip loop
	# that no signal can interrupt. Feeding it a pipe that stays open instead
	# parks it in a plain read: the mode is held, and a signal still lands.
	sleep 120 | modetest -M "${drv:-none}" -s "${conn:-0}" >"$OUT/pattern-modetest.log" 2>&1 &
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
	sleep 1
	kill -KILL "$pattern_pid" 2>/dev/null
	pkill -f 'modetest -M' 2>/dev/null
	run "tail -n 20 $OUT/pattern-modetest.log"
	run "systemctl start gdm"
	note ""
	note "stage 'pattern' done -- log at $LOG"
}

# ---------------------------------------------------------------------------
# Stage: poc  /  poc-check
#
# The proof of one virtual output. asb_drm is the reference implementation of
# the shape the decision picked, so build it unchanged rather than write a
# VMLord module first: if GDM binds it at a resolution hyperv_drm cannot
# offer, and the pointer lands on its own plane, the shape is proven and the
# only work left is the rename and the packaging.
#
# Copy the AppSandbox sources next to this script first:
#
#     scp -r appsandbox/tools/linux/asb_drm  <vm>:/home/<user>/
#
# 'poc' installs and reboots; 'poc-check' is what you run when it comes back,
# at the greeter, not logged in.
# ---------------------------------------------------------------------------
stage_poc() {
	if [ ! -f "$DIR/asb_drm/deploy.sh" ]; then
		note "no $DIR/asb_drm/deploy.sh -- copy the AppSandbox sources here first"
		exit 2
	fi

	say "what the display looks like before the module"
	run "ls -l /dev/dri/"
	drv=$(driver_of_card1)
	note "driver before: ${drv:-none}"

	say "building and installing asb_drm through DKMS"
	run "sh $DIR/asb_drm/deploy.sh 2>&1 | tail -n 40"
	run "dkms status"
	run "cat /etc/modprobe.d/asb_drm.conf"

	say "rebooting so the blacklist and the autoload take effect"
	note "when it comes back, do NOT log in -- run: sudo sh probe.sh poc-check"
	run "sleep 3; systemctl reboot"
}

stage_poc_check() {
	say "did the module take the display"
	run "lsmod | grep -E 'asb_drm|hyperv_drm' || echo 'neither loaded'"
	run "dmesg | grep -iE 'asb|drm' | tail -n 20"
	run "ls -l /dev/dri/"
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		describe_card "$node"
	done

	say "the modes it offers -- the point is the ones hyperv_drm cannot"
	drv=$(driver_of_card1)
	note "driver under test: ${drv:-none}"
	run "modetest -M ${drv:-none} -c 2>&1 | head -n 40"

	say "did GDM bind it before login"
	run "systemctl is-active gdm graphical.target"
	run "loginctl seat-status seat0 2>&1 | head -n 30"
	run "journalctl -b --no-pager | grep -iE 'mutter|Added device|cursor plane|primary GPU' | tail -n 30"

	say "reading the greeter off the module's planes"
	if [ ! -x "$DIR/plane_capture" ]; then
		run "cc -O2 -Wall -o $DIR/plane_capture $DIR/plane_capture.c \$(pkg-config --cflags --libs libdrm)"
	fi
	for node in /dev/dri/card*; do
		[ -e "$node" ] || continue
		run "$DIR/plane_capture $node 60 $OUT/poc-\$(basename $node).ppm"
	done
	run "ls -l $OUT/*.ppm"

	note ""
	note "stage 'poc-check' done -- log at $LOG"
	note "Two things decide it: a mode list that goes past 1920x1080, and a"
	note "plane line saying (cursor) with an fb of its own."
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
	extra)   stage_extra;;
	desktop) stage_desktop;;
	greeter) stage_greeter;;
	pattern) stage_pattern;;
	poc)       stage_poc;;
	poc-check) stage_poc_check;;
	collect) stage_collect;;
	*)
		cat <<'EOF'
usage: sudo sh probe.sh <stage>

  stock     what a VMLord cloud VM's display stack is before any desktop
  extra     install linux-modules-extra and see what hyperv_drm and vkms
            do once they exist (the stock image ships neither)
  desktop   install GNOME + GDM and reboot (destructive to this VM)
  greeter   at the GDM greeter, not logged in: what the compositor bound to
            and whether its framebuffer can be read from outside it
  pattern   with GDM stopped, modetest paints a test pattern and the probe
            reads it back -- separates a broken capture from a blank desktop
  poc       build asb_drm through DKMS and reboot onto it (needs the
            AppSandbox sources copied to ./asb_drm)
  poc-check at the greeter afterwards: what the module gives and what a
            capture reads off it
  collect   tar up /var/log/vmlord-drm-spike for the report
EOF
		exit 2;;
esac
