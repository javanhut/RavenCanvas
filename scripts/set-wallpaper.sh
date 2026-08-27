#!/usr/bin/env bash
# Set the wallpaper this machine shows -- on the desktop and on the login
# screen, which are the same picture by design.
#
# What it touches, and nothing else:
#
#   /usr/share/wallpaper/            the library. `set` copies your image here
#                                    unless it is here already.
#   /usr/share/wallpaper/set/        exactly one entry, named `wallpaper`,
#                                    a symlink into the library.
#
# It writes no config file. A path in canvas.toml or login.toml overrides the
# machine's wallpaper rather than setting it, which is the opposite of what
# this is for -- so `set` warns if it finds one and leaves it alone.
#
# ravencanvasd watches set/ and picks the change up within a moment. There is
# nothing to restart and nothing to log out of.
#
#   sudo ./scripts/set-wallpaper.sh /path/to/image
#   sudo ./scripts/set-wallpaper.sh clear
#   ./scripts/set-wallpaper.sh status          # no root needed
set -euo pipefail

PREFIX="${PREFIX:-/usr}"
LIBRARY="${PREFIX}/share/wallpaper"
SET_DIR="${LIBRARY}/set"

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

need_root() {
    [ "$(id -u)" -eq 0 ] || { echo "set-wallpaper.sh $1 must run as root" >&2; exit 1; }
}

# What is in set/ now, if anything. Mirrors ravencanvasd's own rule: the file
# whose stem is exactly `wallpaper`, whatever the extension, sorted so that a
# directory holding two resolves the same way twice.
current() {
    find "${SET_DIR}" -maxdepth 1 -name 'wallpaper.*' -o -maxdepth 1 -name 'wallpaper' 2>/dev/null |
        sort | head -n 1
}

# Warn about an override rather than removing it -- it is somebody's deliberate
# choice and this script is not the place to overrule it, but silently doing
# nothing visible is worse.
warn_overrides() {
    if [ -f /etc/raven/canvas.toml ] &&
       grep -qE '^\s*\[background\]' /etc/raven/canvas.toml; then
        echo "WARN  /etc/raven/canvas.toml has a [background]; it overrides this"
        echo "      for every user. Comment it out to use the machine wallpaper."
    fi
    if [ -f "${HOME:-}/.config/raven/canvas.toml" ] &&
       grep -qE '^\s*\[background\]' "${HOME}/.config/raven/canvas.toml"; then
        echo "WARN  ~/.config/raven/canvas.toml has a [background]; it overrides"
        echo "      this for you. \`ravencanvas set\` wrote it if you did not."
    fi
    if [ -f /etc/raven/login.toml ] &&
       grep -qE '^\s*wallpaper\s*=' /etc/raven/login.toml; then
        echo "WARN  /etc/raven/login.toml names a wallpaper; the login screen"
        echo "      will keep using that one rather than this."
    fi
}

case "${1:-status}" in
-h|--help|help)
    usage 0
    ;;

status)
    found="$(current || true)"
    if [ -n "${found}" ]; then
        echo "set   ${found}"
        if [ -L "${found}" ]; then
            echo "  ->  $(readlink -f "${found}" 2>/dev/null || echo '(dangling)')"
        fi
    else
        echo "set   nothing; the desktop falls back to the built-in gradient"
    fi
    echo
    echo "library ${LIBRARY}:"
    find "${LIBRARY}" -maxdepth 1 -type f -printf '  %f\n' 2>/dev/null | sort || true
    warn_overrides
    ;;

clear)
    need_root clear
    rm -f "${SET_DIR}"/wallpaper "${SET_DIR}"/wallpaper.*
    echo "ok    cleared ${SET_DIR}; the desktop falls back to the built-in"
    ;;

*)
    need_root set
    image="$1"
    [ -f "${image}" ] || { echo "no such file: ${image}" >&2; exit 1; }

    # Format by content, not by extension -- the same rule the decoder uses, so
    # a .jpg that is really a PNG is accepted here exactly as it will be there.
    case "$(head -c 4 "${image}" | od -An -tx1 | tr -d ' \n')" in
        89504e47) kind=png ;;
        ffd8ff*)  kind=jpg ;;
        *) echo "not a PNG or JPEG: ${image}" >&2; exit 1 ;;
    esac

    install -d -m 0755 "${LIBRARY}" "${SET_DIR}"

    # Into the library first, unless it is already there. The library is the
    # thing that survives; set/ is only a pointer into it.
    target="${image}"
    if [ "$(dirname "$(readlink -f "${image}")")" != "${LIBRARY}" ]; then
        target="${LIBRARY}/$(basename "${image}")"
        install -m 0644 "${image}" "${target}"
        echo "ok    copied into ${LIBRARY}"
    fi

    # World-readable, because two different unprivileged accounts open it: this
    # user's ravencanvasd and the greeter's own account. This is the commonest
    # reason a wallpaper silently does nothing.
    chmod 0644 "${target}"

    rm -f "${SET_DIR}"/wallpaper "${SET_DIR}"/wallpaper.*
    ln -s "../$(basename "${target}")" "${SET_DIR}/wallpaper.${kind}"
    echo "ok    ${SET_DIR}/wallpaper.${kind} -> ../$(basename "${target}")"
    echo
    echo "The desktop should change within a moment; ravencanvasd watches set/."
    echo "The login screen picks it up on its next start."
    warn_overrides
    ;;
esac
