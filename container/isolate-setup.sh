#!/bin/sh
# Prepare an isolate-capable environment inside a (rootless) podman container,
# then exec the given command. Prepend this to the RUN-stage command, e.g.
#   podman run <flags> IMAGE /usr/local/bin/isolate-setup.sh cargo nextest run ...
#
# Best-effort with warnings (not set -e): if a step can't run (e.g. cgroup not
# writable, read-only rootfs), we warn and still exec, so the caller sees
# isolate's own diagnostic rather than the container vanishing.
set -u

warn() { echo "isolate-setup: $*" >&2; }

# --- derive a box UID/GID range that lies inside the container's userns map --
# /proc/self/{uid,gid}_map rows are "inside_start outside_start count". Pick the
# row with the largest count (the subid block) and place the box range inside
# it, so box uids are always mapped -- no assumption about how podman was run
# (default rootless, rootful, or a custom --uidmap). Override with
# ISOLATE_FIRST_UID / ISOLATE_FIRST_GID / ISOLATE_NUM_BOXES.
pick_block() { awk '$3>max{max=$3; s=$1; c=$3} END{print s" "c}' "$1"; }
uid_line=$(pick_block /proc/self/uid_map)
gid_line=$(pick_block /proc/self/gid_map)
u_start=${uid_line% *}; u_count=${uid_line#* }
g_start=${gid_line% *}; g_count=${gid_line#* }

num_boxes=${ISOLATE_NUM_BOXES:-100}

# never assign box uid/gid 0; clamp num_boxes to what both maps can hold
u_lo=$u_start; [ "$u_lo" -lt 1 ] && u_lo=1
g_lo=$g_start; [ "$g_lo" -lt 1 ] && g_lo=1
u_room=$(( u_start + u_count - u_lo ))
g_room=$(( g_start + g_count - g_lo ))
room=$u_room; [ "$g_room" -lt "$room" ] && room=$g_room
[ "$num_boxes" -gt "$room" ] && num_boxes=$room
[ "$num_boxes" -lt 1 ] && warn "WARNING: container uid/gid map has no usable range (uid=$uid_line gid=$gid_line)"

# prefer starting 1000 into the block (skip low system-ish uids) when there is
# room, else start at the lowest usable id
u_base=$u_lo; [ $(( u_start + 1000 + num_boxes )) -le $(( u_start + u_count )) ] && u_base=$(( u_start + 1000 ))
g_base=$g_lo; [ $(( g_start + 1000 + num_boxes )) -le $(( g_start + g_count )) ] && g_base=$(( g_start + 1000 ))
first_uid=${ISOLATE_FIRST_UID:-$u_base}
first_gid=${ISOLATE_FIRST_GID:-$g_base}

# --- write isolate config ---------------------------------------------------
# Replaces the stock `subid_user = isolate` (needs an isolate user in
# /etc/subuid we don't have) and `cg_root = auto:...` (needs the systemd
# cg-keeper we don't run) with an explicit uid block + explicit cg_root.
if ! cat > /usr/local/etc/isolate <<EOF
box_root = /var/local/lib/isolate
lock_root = /run/isolate/locks
cg_root = /sys/fs/cgroup/isolate
first_uid = $first_uid
first_gid = $first_gid
num_boxes = $num_boxes
EOF
then
  warn "WARNING: could not write /usr/local/etc/isolate (read-only rootfs?); isolate will use its stock config"
fi

# --- filesystem prep --------------------------------------------------------
mkdir -p /var/local/lib/isolate /run/isolate/locks 2>/dev/null || true
# isolate refuses a box_root writable by group/other (e.g. a 1777 tmpfs).
chmod 0755 /var/local/lib/isolate 2>/dev/null || true

# --- cgroup prep ------------------------------------------------------------
# cgroup v2's no-internal-process rule: to enable controllers for children of
# the container cgroup root, that cgroup must hold no processes -- so move them
# into a leaf first. Then enable controllers at the root AND on isolate's
# cg_root, so per-box children get memory.max/cpu.max. (On a systemd host this
# is what isolate-cg-keeper/systemd would arrange.)
if [ -w /sys/fs/cgroup/cgroup.subtree_control ]; then
  mkdir -p /sys/fs/cgroup/init 2>/dev/null || true
  while read -r pid; do
    echo "$pid" > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true
  done < /sys/fs/cgroup/cgroup.procs
  if echo "+cpu +memory +pids" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null; then
    mkdir -p /sys/fs/cgroup/isolate 2>/dev/null || true
    echo "+cpu +memory +pids" > /sys/fs/cgroup/isolate/cgroup.subtree_control 2>/dev/null \
      || warn "WARNING: could not enable controllers on cg_root; memory/cpu limits may not apply"
  else
    warn "WARNING: could not enable controllers at cgroup root; memory/cpu limits may not apply"
  fi
else
  warn "WARNING: /sys/fs/cgroup not writable; isolate cgroup limits will not work (needs --cgroupns=private + unmask=/sys/fs/cgroup)"
fi

exec "$@"
