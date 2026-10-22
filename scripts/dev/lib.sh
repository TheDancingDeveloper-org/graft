# Shared setup for scripts/dev/*.sh. Source it; don't run it.
#
# Sets GRAFT_HOST_ROOT: the checkout's path on the Docker daemon's filesystem,
# which compose uses for every bind mount. Locally it is the checkout itself.
# When the daemon is on another host (this estate's vogt-prod checkout talks to
# Node B), the checkout is a bind mount there, so the path is translated from
# the mount's source, and the translation is checked with a real file probe.

graft_dev_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$graft_dev_root"

graft_dev_fail() {
  echo "$(basename "${0:-graft-dev}"): $*" >&2
  exit 1
}

graft_dev_set_host_root() {
  if [ -n "${GRAFT_HOST_ROOT:-}" ]; then
    export GRAFT_HOST_ROOT
    return 0
  fi
  local daemon
  daemon="$(docker info --format '{{.Name}}' 2>/dev/null || true)"
  if [ -z "$daemon" ] || [ "$daemon" = "$(hostname)" ]; then
    export GRAFT_HOST_ROOT="$graft_dev_root"
    return 0
  fi

  # Remote daemon: map this checkout's mount point to its source on the host.
  # findmnt prints e.g. /dev/nvme0n1p1[/vogt/prod/workspace]; the bracketed
  # part is the subpath the bind mount was taken from.
  local src tgt sub host
  src="$(findmnt -T "$graft_dev_root" -no SOURCE 2>/dev/null || true)"
  tgt="$(findmnt -T "$graft_dev_root" -no TARGET 2>/dev/null || true)"
  sub="$(sed -n 's/.*\[\(.*\)\]$/\1/p' <<<"$src")"
  if [ -z "$sub" ] || [ -z "$tgt" ]; then
    graft_dev_fail "the Docker daemon is '$daemon' but this checkout is not a bind mount findmnt can map. Set GRAFT_HOST_ROOT to its path on that daemon's host."
  fi
  host="$sub${graft_dev_root#"$tgt"}"

  # Probe the daemon's own filesystem through a read-only root mount. A bind
  # mount of the missing path would make Docker create an empty root-owned
  # directory there; this only reads. The image is needed for the probe, and
  # up.sh builds it when it is missing.
  if ! docker image inspect graft-dev:local >/dev/null 2>&1; then
    export GRAFT_HOST_ROOT="$host"
    return 0
  fi
  if ! docker run --rm -v /:/host:ro --entrypoint test graft-dev:local -f "/host$host/README.md"; then
    graft_dev_fail "the daemon's filesystem has no $host/README.md; the mapping '$graft_dev_root' -> '$host' is wrong, so bind mounts would not reach this checkout"
  fi
  export GRAFT_HOST_ROOT="$host"
}

graft_dev_set_host_root
