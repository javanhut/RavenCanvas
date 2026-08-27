#!/usr/bin/env bash
# Install RavenCanvas: the two binaries, the reference config, the wallpaper
# directories, and the one line that starts the daemon with your session.
#
# Idempotent. Run it again after a rebuild to replace the binaries without
# touching a config file you have edited or wiring anything twice.
#
# Unlike RavenLogin's installer this touches no accounts, no init services and
# no kernel cmdline. ravencanvasd is an ordinary unprivileged Wayland client
# that runs as you: the only privileged thing here is writing to /usr.
#
#   sudo ./scripts/install.sh              build output -> /usr, wire the session
#   sudo WIRE_SESSION=0 ./scripts/install.sh    binaries and config only
#   sudo ./scripts/uninstall.sh            undo all of it
set -euo pipefail

PREFIX="${PREFIX:-/usr}"
SYSCONFDIR="${SYSCONFDIR:-/etc}"
SESSION="${SESSION:-${PREFIX}/sbin/raven-wayland-session}"

# The marker the wiring is bracketed by. `uninstall.sh` finds the block by
# these two lines, so they are a contract between the two scripts and must not
# be reworded on one side alone.
BEGIN_MARK="# >>> ravencanvas >>>"
END_MARK="# <<< ravencanvas <<<"

cd "$(dirname "$0")/.."

[ "$(id -u)" -eq 0 ] || { echo "install.sh must run as root" >&2; exit 1; }

# --- binaries --------------------------------------------------------------
if [ ! -x target/release/ravencanvasd ] || [ ! -x target/release/ravencanvas ]; then
    echo "Build first: cargo build --release" >&2
    exit 1
fi

# Renamed over rather than written through. You cannot write to a running
# executable -- the kernel returns ETXTBSY -- and ravencanvasd is running
# whenever you are looking at a wallpaper it drew. `install` writes a new file
# and renames, so the running daemon keeps the inode it started with and
# carries on undisturbed until it is next restarted.
install -D -m 0755 target/release/ravencanvasd "${PREFIX}/bin/ravencanvasd"
install -D -m 0755 target/release/ravencanvas  "${PREFIX}/bin/ravencanvas"
echo "ok    installed ravencanvasd and ravencanvas into ${PREFIX}/bin"

install -D -m 0755 scripts/set-wallpaper.sh "${PREFIX}/bin/raven-set-wallpaper"
echo "ok    installed raven-set-wallpaper into ${PREFIX}/bin"

# --- config ----------------------------------------------------------------
#
# Never overwritten. Every value in it is a default the daemon already has
# compiled in, so a machine without this file behaves identically and there is
# nothing to gain by clobbering one somebody has edited.
#
# Note that the shipped file has [background] commented out, and that this is
# load-bearing rather than an omission: a [background] here would override the
# machine's wallpaper for every user and break the contract with the login
# screen. See the long comment at the top of config/canvas.toml.
if [ -f "${SYSCONFDIR}/raven/canvas.toml" ]; then
    echo "ok    ${SYSCONFDIR}/raven/canvas.toml exists; leaving it alone"
    if grep -qE '^\s*\[background\]' "${SYSCONFDIR}/raven/canvas.toml"; then
        echo "WARN  ...but it has a [background], which overrides the machine's"
        echo "      wallpaper for every user on this machine. That is supported"
        echo "      and may be what you want; it is not the default."
    fi
else
    install -D -m 0644 config/canvas.toml "${SYSCONFDIR}/raven/canvas.toml"
    echo "ok    installed ${SYSCONFDIR}/raven/canvas.toml"
fi

# --- the wallpaper directories ---------------------------------------------
#
# Created empty, and nothing is installed into them. `set/` is the contract
# between this daemon and RavenLogin's greeter: whatever is in it is what both
# of them draw. RavenLogin's installer creates the same two directories, and
# both doing it is deliberate -- either project may be installed first.
install -d -m 0755 "${PREFIX}/share/wallpaper" "${PREFIX}/share/wallpaper/set"
echo "ok    ${PREFIX}/share/wallpaper and set/ exist"

# --- starting it with the session ------------------------------------------
#
# ravencanvasd has to start before the compositor it connects to, because the
# alternative is a start-order race -- so it is backgrounded ahead of the
# session script's final `exec`, which is the same shape that script already
# uses for its dbus-daemon. `connect()` retries for ten seconds, which is what
# makes starting first safe rather than merely early.
#
# Appended to raven-wayland-session rather than shipped as a service: this is a
# session client running as the logged-in user, and raven-init only knows about
# system services running as root.
WIRE_SESSION="${WIRE_SESSION:-1}"

if [ "${WIRE_SESSION}" != "1" ]; then
    echo "..    WIRE_SESSION=0; leaving ${SESSION} alone"

elif [ ! -f "${SESSION}" ]; then
    echo "WARN  ${SESSION} does not exist; not wiring."
    echo "      Start the daemon by hand, or from whatever starts your session:"
    echo "          ravencanvasd &"

elif grep -qF "${BEGIN_MARK}" "${SESSION}"; then
    echo "ok    ${SESSION} already starts ravencanvasd"

elif ! grep -qE '^exec .*(COMPOSITOR|huginn)' "${SESSION}"; then
    # Refuse to guess. If the script does not end in the exec this expects,
    # inserting before "the last exec" could put the daemon somewhere that
    # never runs, or worse, before an early exit.
    echo "WARN  ${SESSION} has no recognisable compositor exec; not wiring."
    echo "      Add this yourself, immediately before the exec at the end:"
    echo "          ${PREFIX}/bin/ravencanvasd &"

else
    cp -p "${SESSION}" "${SESSION}.pre-ravencanvas"

    # Inserted before the exec line rather than appended, because everything
    # after an exec is unreachable. awk over sed: this needs to fire on the
    # first match only, and a session script that grew a second exec should
    # still get exactly one daemon.
    tmp="$(mktemp)"
    awk -v begin="${BEGIN_MARK}" -v end="${END_MARK}" -v bin="${PREFIX}/bin/ravencanvasd" '
        !done && /^exec / {
            # A blank line first, so the block reads as its own paragraph
            # rather than as the tail of the comment that documents the exec.
            # uninstall.sh drops it again; the two are tested by round-tripping
            # the real session script.
            print ""
            print begin
            print "# The wallpaper. Backgrounded before the exec below, like the session"
            print "# bus above it: it must be running before the compositor it connects"
            print "# to, and it retries the connection for ten seconds so that starting"
            print "# first is safe. Removed by RavenCanvas'"'"'s uninstall.sh."
            print "if command -v " bin " >/dev/null 2>&1; then"
            print "    " bin " &"
            print "fi"
            print end
            done = 1
        }
        { print }
    ' "${SESSION}" > "${tmp}"

    # Prove the edit did what it claims before it replaces anything. A session
    # script that no longer execs the compositor is a machine that boots to
    # nothing, and that is not a thing to find out about at the next reboot.
    if ! grep -qF "${BEGIN_MARK}" "${tmp}" || ! grep -qE '^exec ' "${tmp}"; then
        rm -f "${tmp}"
        echo "WARN  the edit to ${SESSION} did not come out right; left it alone."
        exit 1
    fi

    cat "${tmp}" > "${SESSION}"
    rm -f "${tmp}"
    chmod --reference="${SESSION}.pre-ravencanvas" "${SESSION}"
    echo "ok    ${SESSION} now starts ravencanvasd"
    echo "      (backup: ${SESSION}.pre-ravencanvas)"
fi

# --- what is left ----------------------------------------------------------
cat <<NEXT

Installed. To see it without logging out, start the daemon in the session you
are already in:

    ravencanvasd &

Then give it a picture. This is the machine's wallpaper, so the login screen
gets the same one:

    sudo raven-set-wallpaper /path/to/image

With nothing in /usr/share/wallpaper/set the desktop draws the built-in
gradient, which is also what you get if the picture will not decode.

To undo all of it: sudo ./scripts/uninstall.sh
NEXT
