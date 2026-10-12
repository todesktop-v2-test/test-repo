#!/bin/sh
# Runs install.sh (the live one, or a file given as $1) through the scenarios
# the install-script workflow checks, and prints what happened in each.
set -u
EDGE=https://v2-edge-dev.todesktop.workers.dev
APP=261012lntx26f
SCRIPT=${1:-}
if [ -z "$SCRIPT" ]; then
  SCRIPT=$(mktemp); curl -fsSL "$EDGE/$APP/install.sh" -o "$SCRIPT"
fi
run() { # run NAME [ENV=...] -- runs the script with --tarball, prints the exit code
  name=$1; shift
  echo "::group::$name"
  env "$@" sh -x "$SCRIPT" --tarball > /tmp/run.log 2>&1
  echo "exit=$?"; tail -n 25 /tmp/run.log
  echo "::endgroup::"
}
state() {
  echo "--- $1"
  systemctl --user is-enabled "$APP.service" 2>&1; systemctl --user is-active "$APP.service" 2>&1
  systemctl --user show -p MainPID -p ActiveEnterTimestamp -p FragmentPath "$APP.service" 2>&1
  ls -la "$HOME/.local/share/$APP" 2>&1 | tail -n +2
}
clean() {
  systemctl --user disable --now "$APP.service" >/dev/null 2>&1
  rm -rf "$HOME/.local/share/$APP" "$HOME/.config/systemd/user/$APP.service" "$HOME/fleet" "$HOME/xdgconf"
  systemctl --user daemon-reload 2>/dev/null
}

echo "=== environment"
id; systemctl --version | head -1
echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR-unset} DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS-unset} XDG_CONFIG_HOME=${XDG_CONFIG_HOME-unset}"
loginctl show-user "$(id -u)" 2>&1 | grep -E 'Linger|State|Sessions' || true
systemctl --user show-environment 2>&1 | head -5; echo "show-environment exit=$?"
clean

echo "=== 1. HOME overridden (the fleet test), 5 times"
for i in 1 2 3 4 5; do
  mkdir -p "$HOME/fleet/$i"
  run "fleet $i" HOME="$HOME/fleet/$i"
  echo "fleet $i: $(HOME=$HOME/fleet/$i "$HOME/fleet/$i/.local/bin/acme-agent" --version 2>&1)"
done
ls -la "$HOME/fleet/1/.config/systemd/user" 2>&1
clean

echo "=== 2. plain user install"
state before
echo "::group::plain"; sh "$SCRIPT" --tarball 2>&1; echo "exit=$?"; echo "::endgroup::"
sleep 3; state "after install"
journalctl --user -u "$APP.service" -n 5 --no-pager 2>&1 | tail -n 5

echo "=== 3. re-run while the service runs (upgrade)"
sh "$SCRIPT" --tarball 2>&1; echo "exit=$?"
sleep 3; state "after re-run"
cat "$HOME/.local/share/$APP/install.json"

echo "=== 3b. re-run while an older build runs (upgrade)"
R="$HOME/.local/share/$APP"; B=$(readlink "$R/current" | sed 's#versions/##')
cp -a "$R/versions/$B" "$R/versions/b0-older"; ln -sfn versions/b0-older "$R/current"; rm -rf "$R/versions/$B"
systemctl --user restart "$APP.service"; sleep 2
P=$(systemctl --user show -p MainPID --value "$APP.service"); echo "before: $P $(readlink /proc/$P/exe)"
sh "$SCRIPT" --tarball 2>&1; echo "exit=$?"
sleep 2; P=$(systemctl --user show -p MainPID --value "$APP.service"); echo "after: $P $(readlink /proc/$P/exe)"
ls -a "$R/versions"; cat "$R/install.json"
clean

echo "=== 4. XDG_CONFIG_HOME elsewhere"
mkdir -p "$HOME/xdgconf"
XDG_CONFIG_HOME="$HOME/xdgconf" sh "$SCRIPT" --tarball 2>&1; echo "exit=$?"
sleep 2; state "after XDG_CONFIG_HOME install"
ls -la "$HOME/xdgconf/systemd/user" "$HOME/.config/systemd/user" 2>&1
clean

echo "=== 5. as root with --tarball"
sudo sh "$SCRIPT" --tarball 2>&1; echo "exit=$?"
sudo systemctl is-enabled "$APP.service" 2>&1
sudo systemctl show -p ActiveState -p MainPID -p FragmentPath "$APP.service" 2>&1
sudo journalctl -u "$APP.service" -n 3 --no-pager 2>&1
echo "--- root re-run"
sudo sh "$SCRIPT" --tarball 2>&1; echo "exit=$?"
sudo systemctl show -p ActiveState -p MainPID "$APP.service" 2>&1
sudo systemctl disable --now "$APP.service" 2>&1
true
