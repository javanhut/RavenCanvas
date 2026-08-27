#!/usr/bin/env bash
# Undo scripts/install.sh.
#
# Removes the binaries and unwires the session. It does NOT remove
# /etc/raven/canvas.toml, and it does NOT touch /usr/share/wallpaper -- your
# pictures and your choice of wallpaper are not this script's to throw away,
# and the login screen is still reading set/ after RavenCanvas is gone.
#
#   sudo ./scripts/uninstall.sh
#   sudo ./scripts/uninstall.sh --purge     ...also remove canvas.toml
set -euo pipefail

PREFIX="${PREFIX:-/usr}"
SYSCONFDIR="${SYSCONFDIR:-/etc}"
SESSION="${SESSION:-${PREFIX}/sbin/raven-wayland-session}"

BEGIN_MARK="# >>> ravencanvas >>>"
END_MARK="# <<< ravencanvas <<<"

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

[ "$(id -u)" -eq 0 ] || { echo "uninstall.sh must run as root" >&2; exit 1; }

# --- unwire the session ----------------------------------------------------
#
# By the markers rather than by restoring the backup. A backup taken at install
# time is a snapshot of a file that may have been legitimately updated since --
# restoring it would silently undo somebody else's change to the session script.
# Cutting out the block this script put there removes exactly what was added.
if [ -f "${SESSION}" ] && grep -qF "${BEGIN_MARK}" "${SESSION}"; then
    tmp="$(mktemp)"
    # Blank lines are held rather than printed, so that the one install.sh put
    # *before* the block goes with it: by the time the begin marker is seen the
    # blank has not been printed yet, and resetting the counter drops it. Any
    # other blank line is flushed unchanged before the next real line, so this
    # round-trips a session script byte for byte -- which is the property the
    # two scripts are tested on.
    awk -v begin="${BEGIN_MARK}" -v end="${END_MARK}" '
        $0 == begin { skipping = 1; held = 0; next }
        $0 == end   { skipping = 0; next }
        skipping    { next }
        /^$/        { held++; next }
        { while (held > 0) { print ""; held-- } print }
        END         { while (held > 0) { print ""; held-- } }
    ' "${SESSION}" > "${tmp}"

    if ! grep -qE '^exec ' "${tmp}"; then
        rm -f "${tmp}"
        echo "WARN  unwiring would have removed the compositor exec from"
        echo "      ${SESSION}; left it alone. Check it by hand."
    else
        cat "${tmp}" > "${SESSION}"
        rm -f "${tmp}"
        echo "ok    ${SESSION} no longer starts ravencanvasd"
    fi
elif [ -f "${SESSION}" ]; then
    echo "ok    ${SESSION} does not start ravencanvasd"
fi

# The backup is left behind on purpose: it is the only copy of what the session
# script looked like before, and removing it is the one step that cannot be
# undone. Named so it is obvious what it is and safe to delete.
if [ -f "${SESSION}.pre-ravencanvas" ]; then
    echo "..    ${SESSION}.pre-ravencanvas kept; delete it when you are happy"
fi

# --- binaries --------------------------------------------------------------
for binary in ravencanvasd ravencanvas raven-set-wallpaper; do
    if [ -e "${PREFIX}/bin/${binary}" ]; then
        rm -f "${PREFIX}/bin/${binary}"
        echo "ok    removed ${PREFIX}/bin/${binary}"
    fi
done

# --- config, only if asked -------------------------------------------------
if [ "${PURGE}" = "1" ]; then
    if [ -f "${SYSCONFDIR}/raven/canvas.toml" ]; then
        rm -f "${SYSCONFDIR}/raven/canvas.toml"
        echo "ok    removed ${SYSCONFDIR}/raven/canvas.toml"
    fi
    echo "..    ~/.config/raven/canvas.toml is yours; not touched"
else
    echo "..    ${SYSCONFDIR}/raven/canvas.toml kept (--purge removes it)"
fi

echo "..    ${PREFIX}/share/wallpaper kept -- the login screen still reads it"

cat <<NEXT

Uninstalled. A ravencanvasd already running is still running and still holding
its layer surface -- there is no request that asks it to quit, by design, so it
goes when the session does, or now:

    pkill -x ravencanvasd
NEXT
