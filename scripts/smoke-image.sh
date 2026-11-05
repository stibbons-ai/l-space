#!/usr/bin/env bash
# Smoke-tests a Clacks (Bridge) image the way the README runs it: host networking, a named data
# volume, the configuration file mounted read-only, and the encryption key as a secret file.
#
#   scripts/smoke-image.sh <image>
#
# Needs docker, curl, jq and openssl. Cleans up every container, volume and file it creates.
set -euo pipefail

image=${1:?usage: scripts/smoke-image.sh <image>}
run_id="clacks-smoke-$$"
volume="$run_id-data"
work=$(mktemp -d)
containers=()

cleanup() {
  if ((${#containers[@]})); then docker rm --force "${containers[@]}" >/dev/null 2>&1 || true; fi
  docker volume rm --force "$volume" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Port 0: the Bridge picks a free port on the host and logs it in its `listening` event.
cat >"$work/bridge.toml" <<'EOF'
listen = "127.0.0.1:0"
database = "/var/lib/clacks/bridge.db"

[[library_servers]]
url = "https://library.example.com"
name = "Smoke"
EOF
# A throwaway key, made for this run only. The container runs as UID 65532, which can't chown
# without root, so the file is world-readable inside this private temporary directory.
openssl rand -base64 32 >"$work/bridge_encryption_key"
chmod 0644 "$work/bridge.toml" "$work/bridge_encryption_key"

# Starts a Bridge container on the shared volume; extra `docker run` arguments follow the name.
start() {
  local name="$run_id-$1"
  shift
  containers+=("$name")
  docker run --detach --name "$name" --network host \
    --volume "$volume:/var/lib/clacks" \
    --mount "type=bind,source=$work/bridge.toml,target=/etc/clacks/bridge.toml,readonly" \
    "$@" "$image" >/dev/null
}

with_key=(
  --mount "type=bind,source=$work/bridge_encryption_key,target=/run/secrets/bridge_encryption_key,readonly"
  --env BRIDGE_ENCRYPTION_KEY_FILE=/run/secrets/bridge_encryption_key
)

# Waits for container `$1`'s `listening` event and prints the address it logged.
listening_addr() {
  local name="$run_id-$1" addr
  for _ in $(seq 1 60); do
    addr=$(docker logs "$name" 2>/dev/null | jq -r 'select(.message == "listening") | .addr' 2>/dev/null || true)
    if [[ -n $addr ]]; then
      echo "$addr"
      return
    fi
    if [[ $(docker inspect --format '{{.State.Running}}' "$name") != true ]]; then
      docker logs "$name" >&2
      fail "$1 exited before listening"
    fi
    sleep 1
  done
  docker logs "$name" >&2
  fail "$1 did not log 'listening' within 60 s"
}

# Checks the Bridge at `$1` lists exactly the configured Library Server. Retries a refused
# connection: on a Mac, OrbStack forwards a host-network port a moment after the Bridge binds it.
check_library_servers() {
  local body
  body=$(curl --silent --show-error --fail --retry 5 --retry-delay 1 --retry-connrefused \
    "http://$1/api/library-servers") || fail "GET /api/library-servers"
  jq --exit-status '. == [{"base_url": "https://library.example.com", "name": "Smoke"}]' <<<"$body" >/dev/null \
    || fail "unexpected /api/library-servers: $body"
}

# Checks the volume holds the database, and that the Bridge's user (65532) owns both and alone
# can read them.
check_database() {
  local listing dir db
  # GNU tar lists `-rw------- 65532/65532 …`, bsdtar `-rw-------  0 65532  65532 …`.
  local owned='65532[/ ]+65532 '
  listing=$(docker cp "$run_id-$1:/var/lib/clacks" - | tar --numeric-owner -tvf -) \
    || fail "cannot read the volume"
  dir=$(grep -E ' clacks/?$' <<<"$listing") || fail "no data directory in: $listing"
  db=$(grep -E ' clacks/bridge\.db$' <<<"$listing") || fail "no database in the volume: $listing"
  [[ $dir =~ ^drwx------\ .*$owned ]] || fail "unexpected data directory: $dir"
  [[ $db =~ ^-rw-------\ .*$owned ]] || fail "unexpected database file: $db"
}

echo "== $image: first start on a fresh volume"
start first "${with_key[@]}"
addr=$(listening_addr first)
check_library_servers "$addr"
check_database first
echo "ok: listening on $addr, Library Server listed, database created in the volume"

echo "== restart: a new container on the same volume"
# The Bridge, as PID 1 without a SIGTERM handler, ignores `docker stop`'s SIGTERM; don't wait
# the 10 s grace period for the SIGKILL that follows.
docker stop --time 0 "$run_id-first" >/dev/null
start second "${with_key[@]}"
addr=$(listening_addr second)
check_library_servers "$addr"
check_database second
echo "ok: back up on $addr with the existing database"

echo "== no encryption key: exits 1"
start no-key
status=$(docker wait "$run_id-no-key")
[[ $status == 1 ]] || fail "exit code $status without a key, expected 1"
docker logs "$run_id-no-key" | jq --exit-status 'select(.message == "cannot load the encryption key")' >/dev/null \
  || fail "no 'cannot load the encryption key' event"
echo "ok: exit code 1 with 'cannot load the encryption key'"

echo "PASS: $image"
