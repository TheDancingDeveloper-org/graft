#!/usr/bin/env bash
# Build or load the graft image, run examples/compose, then login, clone,
# commit and land one file. The bootstrap token stays in a mode-0600 file and
# is passed only as --token-file. set -x stays off.
set -euo pipefail

root=$(cd -- "$(dirname -- "$0")/.." && pwd)
image=${GRAFT_IMAGE:-graft-compose-smoke:local}
project=${COMPOSE_PROJECT_NAME:-graft-smoke-$$}
port=${GRAFT_SERVER_PORT:-17878}
metrics=${GRAFT_METRICS_PORT:-19474}
repo=${GRAFT_SMOKE_REPO:-example}

workdir=$(mktemp -d)
token=$workdir/admin.token
log=$workdir/log.txt
install -m 0600 /dev/null "$token"
cp "$root/examples/compose/compose.yaml" "$workdir/compose.yaml"
cp "$root/examples/compose/server.toml" "$workdir/server.toml"
cp "$root/examples/compose/node.toml" "$workdir/node.toml"
# The compose file mounts the configs as files. A bind of a path the daemon
# cannot see becomes a directory, so the configs go through a named volume
# filled over the Docker API. The published files are not edited.
python3 - "$workdir/compose.yaml" << 'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
text = text.replace(
    "./server.toml:/etc/example/server.toml:ro",
    "graft-smoke-config:/etc/example:ro",
)
lines = [
    line for line in text.splitlines(keepends=True)
    if "./node.toml:/etc/example/node.toml:ro" not in line
]
text = "".join(lines)
if "graft-smoke-config:" not in text:
    raise SystemExit("config volume was not rewritten")
needle = "\nvolumes:\n  graft-data:\n"
insert = "\nvolumes:\n  graft-smoke-config:\n  graft-data:\n"
if needle not in text:
    raise SystemExit("top-level volumes block was not found")
text = text.replace(needle, insert, 1)
path.write_text(text)
PY
chmod 0755 "$workdir"
chmod 0644 "$workdir/compose.yaml" "$workdir/server.toml" "$workdir/node.toml"

cleanup() {
  local status=$?
  docker rm -f "graft-smoke-config-${project}" >/dev/null 2>&1 || true
  docker compose -p "$project" -f "$workdir/compose.yaml" down -v --remove-orphans \
    >/dev/null 2>&1 || true
  rm -rf "$workdir"
  exit "$status"
}
trap cleanup EXIT

if [[ -n ${GRAFT_IMAGE:-} ]]; then
  if ! docker image inspect "$image" >/dev/null 2>&1; then
    echo "compose-smoke: image not present locally" >&2
    exit 1
  fi
else
  docker build -f "$root/docker/Dockerfile" -t "$image" "$root"
fi

export GRAFT_IMAGE=$image
export GRAFT_SERVER_PORT=$port
export GRAFT_METRICS_PORT=$metrics

# The client runs on the compose network. Published ports bind the Docker
# host loopback, which is not this process when the daemon is remote.
endpoint="http://server:7878"

wait_ready() {
  local i line
  for i in $(seq 1 60); do
    line=$(
      docker run --rm --network "${project}_graft" --entrypoint bash "$image" \
        -c "exec 3<>/dev/tcp/server/9464 && printf 'GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' >&3 && head -n1 <&3" \
        2>/dev/null || true
    )
    case "$line" in
      *" 200 "*) return 0 ;;
    esac
    sleep 2
  done
  echo "compose-smoke: /readyz did not return 200" >&2
  docker compose -p "$project" -f "$workdir/compose.yaml" logs --tail 40 >&2 || true
  exit 1
}

docker volume create "${project}_graft-smoke-config" >/dev/null
docker run -d --name "graft-smoke-config-${project}" \
  -v "${project}_graft-smoke-config:/cfg" \
  alpine:3.20 sleep 120 >/dev/null
docker cp "$workdir/server.toml" "graft-smoke-config-${project}:/cfg/server.toml"
docker cp "$workdir/node.toml" "graft-smoke-config-${project}:/cfg/node.toml"
docker exec "graft-smoke-config-${project}" chmod 0644 /cfg/server.toml /cfg/node.toml
docker rm -f "graft-smoke-config-${project}" >/dev/null

# Compose creates the state volumes as root. The server runs as uid 10001
# and must create the bootstrap token inside the secrets volume.
for vol in graft-data graft-meta graft-secrets graft-cache; do
  docker volume create "${project}_${vol}" >/dev/null
done
docker run --rm \
  -v "${project}_graft-data:/obj" \
  -v "${project}_graft-meta:/meta" \
  -v "${project}_graft-secrets:/sec" \
  -v "${project}_graft-cache:/work" \
  alpine:3.20 chown 10001:10001 /obj /meta /sec /work

docker compose -p "$project" -f "$workdir/compose.yaml" up -d server
wait_ready

cid=$(docker compose -p "$project" -f "$workdir/compose.yaml" ps -q server)
if [[ -z $cid ]]; then
  echo "compose-smoke: server container is not running" >&2
  exit 1
fi
docker cp "$cid:/var/lib/graft/secrets/admin.token" "$token"
chmod 0600 "$token"
if [[ ! -s $token ]]; then
  echo "compose-smoke: bootstrap token file is empty" >&2
  exit 1
fi

docker run -d --name "graft-smoke-config-${project}" \
  -v "${project}_graft-secrets:/sec" \
  alpine:3.20 sleep 60 >/dev/null
docker cp "$token" "graft-smoke-config-${project}:/sec/client.token"
docker exec "graft-smoke-config-${project}" chown 10001:10001 /sec/client.token
docker exec "graft-smoke-config-${project}" chmod 0600 /sec/client.token
docker rm -f "graft-smoke-config-${project}" >/dev/null

client_run() {
  docker run --rm --network "${project}_graft" \
    --user 10001:10001 \
    -v "${project}_graft-secrets:/sec:ro" \
    -v "${project}_graft-cache:/work" \
    -w /work \
    -e GRAFT_AUTHOR_NAME=smoke \
    -e GRAFT_AUTHOR_EMAIL=smoke@example.invalid \
    --entrypoint graft "$image" \
    --endpoint "$endpoint" --repo "$repo" --token-file /sec/client.token \
    "$@"
}

# bootstrap_admin_repos only scopes the token. Create the repository, its
# canonical empty root and line main before clone. The token stays in the
# container and is not printed.
docker run --rm --network "${project}_graft" \
  --user 10001:10001 \
  -v "${project}_graft-secrets:/sec:ro" \
  -e GRAFT_SMOKE_REPO="$repo" \
  --entrypoint perl "$image" -e '
use strict;
use warnings;
use IO::Socket::INET;

sub grpc {
  my ($path, $body) = @_;
  my $frame = pack("C N", 0, length($body)) . $body;
  my $sock = IO::Socket::INET->new(
    PeerHost => "server", PeerPort => 7878, Proto => "tcp", Timeout => 10,
  ) or die "connect: $!\n";
  $sock->autoflush(1);
  my $token = $ENV{GRAFT_SMOKE_TOKEN};
  my $headers = join("\r\n",
    "POST $path HTTP/1.1",
    "Host: server:7878",
    "Content-Type: application/grpc",
    "te: trailers",
    "graft-protocol: 0.1",
    "Authorization: Bearer $token",
    "Content-Length: " . length($frame),
    "Connection: close",
    "", "");
  $sock->print($headers . $frame) or die "send: $!\n";
  my $data = "";
  while (1) {
    my $n = sysread($sock, my $chunk, 4096);
    last if !defined $n || $n == 0;
    $data .= $chunk;
  }
  close $sock;
  my $flat = $data;
  $flat =~ s/ //g;
  if ($flat =~ /grpc-status:(?!0\b)/) {
    my @lines = grep { !/authorization/i && !/bearer /i } split /\r\n/, $data;
    my $tail = join("\n", @lines > 8 ? @lines[-8 .. -1] : @lines);
    die "compose-smoke: $path failed\n$tail\n";
  }
  my $bin = "";
  while ($data =~ /\r\n\r\n(.*)\z/s) { $data = $1; last }
  # Chunked: size\r\n payload \r\n ... 0\r\n
  my $rest = $data;
  while ($rest =~ /\A([0-9a-fA-F]+)\r\n/s) {
    my $n = hex($1);
    $rest = substr($rest, length($1) + 2);
    last if $n == 0;
    $bin .= substr($rest, 0, $n);
    $rest = substr($rest, $n);
    $rest =~ s/\A\r\n//;
  }
  return $bin;
}

sub field_str {
  my ($n, $s) = @_;
  die "field too long\n" if length($s) > 127;
  return pack("C C", ($n << 3) | 2, length($s)) . $s;
}
sub field_bytes {
  my ($n, $b) = @_;
  die "bytes too long\n" if length($b) > 127;
  return pack("C C", ($n << 3) | 2, length($b)) . $b;
}
sub field_varint {
  my ($n, $v) = @_;
  return pack("C C", ($n << 3), $v);
}

my $name = $ENV{GRAFT_SMOKE_REPO} // "";
die "repo name is empty or too long\n" if $name eq "" || length($name) > 127;
open my $fh, "<", "/sec/client.token" or die "token file: $!\n";
my $token = do { local $/; <$fh> };
close $fh;
$token =~ s/\s+\z//;
die "token file is empty\n" if $token eq "";
$ENV{GRAFT_SMOKE_TOKEN} = $token;

grpc("/graft.v1.AdminService/CreateRepo", field_str(1, $name));

# Canonical empty root (golden vector). Author kind BOT is 3.
my $root_id = pack("H*", "64afdc2aa9e1166e5d4c23a11d34f73bee99f9cb7a46e0a9e5ea8e37ba44bc9b");
my $author = field_varint(3, 3);
my $stamp = field_varint(1, 0);
my $cs = field_str(1, $name) . field_bytes(4, $author) . field_bytes(5, $stamp);
grpc("/graft.v1.RepoService/CreateChangeset", $cs);
grpc("/graft.v1.RepoService/CreateLine", field_str(1, $name) . field_str(2, "main") . field_bytes(3, $root_id));
'
client_run login smoke
client_run clone "$repo" /work --line main
docker run --rm --user 10001:10001 \
  -v "${project}_graft-cache:/work" \
  alpine:3.20 sh -c 'printf "compose smoke\n" > /work/smoke.txt'
client_run commit -m "compose smoke" --meta agent.work_item=WI-2556
client_run land
client_run log | tee "$log"

if ! grep -q "compose smoke" "$log"; then
  echo "compose-smoke: graft log does not show the landed commit" >&2
  exit 1
fi
echo "compose-smoke: ok"
