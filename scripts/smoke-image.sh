#!/usr/bin/env bash
# Smoke-tests a Clacks (Bridge) image the way the README's compose file runs it: host
# networking, an init process, a named data volume, the configuration file mounted read-only,
# and the encryption key as a secret file.
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
# A throwaway key, made for this run only. As the README says, it's readable only by the
# container's user (65532); chowning to that user takes root, so without passwordless sudo (as
# on a developer's Mac) the key is world-readable instead, inside this private temporary
# directory.
if sudo -n true 2>/dev/null; then
  (umask 077 && openssl rand -base64 32 >"$work/bridge_encryption_key")
  sudo chown 65532:65532 "$work/bridge_encryption_key"
else
  openssl rand -base64 32 >"$work/bridge_encryption_key"
  chmod 0644 "$work/bridge_encryption_key"
fi
chmod 0644 "$work/bridge.toml"

# Starts a Bridge container on the shared volume; extra `docker run` arguments follow the name.
start() {
  local name="$run_id-$1"
  shift
  containers+=("$name")
  docker run --detach --name "$name" --network host --init \
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
  body=$(curl --silent --show-error --fail --max-time 10 --retry 5 --retry-delay 1 --retry-connrefused \
    "http://$1/api/library-servers") || fail "GET /api/library-servers"
  jq --exit-status '. == [{"base_url": "https://library.example.com", "name": "Smoke"}]' <<<"$body" >/dev/null \
    || fail "unexpected /api/library-servers: $body"
}

# Checks the volume holds the database, and that the Bridge's user (65532) owns both and alone
# can read them. Prints the volume's listing.
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
  echo "$listing"
}

# Waits up to `$2` seconds for container `$1` to exit and prints its exit code.
exit_code() {
  local name="$run_id-$1"
  for _ in $(seq 1 "$2"); do
    if [[ $(docker inspect --format '{{.State.Running}}' "$name") != true ]]; then
      docker inspect --format '{{.State.ExitCode}}' "$name"
      return
    fi
    sleep 1
  done
  docker logs "$name" >&2
  fail "$1 still running after $2 s"
}

echo "== $image: first start on a fresh volume"
start first "${with_key[@]}"
addr=$(listening_addr first)
check_library_servers "$addr"
check_database first >/dev/null
echo "ok: listening on $addr, Library Server listed, database created in the volume"

echo "== restart: a new container on the same volume"
# The init process passes `docker stop`'s SIGTERM to the Bridge, which has no handler for it
# yet, so it ends at once (exit 143) instead of after the 10 s grace period and a SIGKILL.
SECONDS=0
docker stop "$run_id-first" >/dev/null
stopped_in=$SECONDS
status=$(exit_code first 1)
((stopped_in <= 5)) || fail "docker stop took ${stopped_in} s"
[[ $status == 143 ]] || fail "exit code $status after docker stop, expected 143 (SIGTERM)"
# A marker the second container must see, proving it runs on the first one's volume.
touch "$work/volume-marker"
COPYFILE_DISABLE=1 tar -cf - -C "$work" volume-marker | docker cp - "$run_id-first:/var/lib/clacks/" \
  || fail "cannot write a marker into the volume"
start second "${with_key[@]}"
addr=$(listening_addr second)
check_library_servers "$addr"
listing=$(check_database second)
grep -qE ' clacks/volume-marker$' <<<"$listing" || fail "second container has another volume: $listing"
echo "ok: stopped in ${stopped_in} s (exit $status); back up on $addr on the same volume and database"

echo "== no encryption key: exits 1"
start no-key
status=$(exit_code no-key 30)
[[ $status == 1 ]] || fail "exit code $status without a key, expected 1"
docker logs "$run_id-no-key" | jq --exit-status 'select(.message == "cannot load the encryption key")' >/dev/null \
  || fail "no 'cannot load the encryption key' event"
echo "ok: exit code 1 with 'cannot load the encryption key'"

echo "PASS: $image"
