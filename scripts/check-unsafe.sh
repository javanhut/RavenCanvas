#!/usr/bin/env bash
# Prove that no crate in this workspace has escaped the unsafe forbid.
#
# The root manifest sets `unsafe_code = "forbid"` in [workspace.lints.rust],
# but a lint table only reaches a crate that opts in with `[lints] workspace =
# true`. Dropping that one line from a manifest is therefore all it takes to
# make unsafe compile again, and it is a two-word diff nobody notices in a
# review. This is the check that notices.
#
# Unlike RavenLogin and RavenGUI there is no quarantine crate here and there
# should never be one, so this has no exceptions list: every member opts in, or
# this fails. See the note in Cargo.toml for why -- this process decodes images
# off disk and writes into a buffer the compositor also maps.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0

for manifest in crates/*/Cargo.toml; do
    crate="$(basename "$(dirname "${manifest}")")"

    if ! grep -qE '^\s*workspace\s*=\s*true' <(sed -n '/^\[lints\]/,/^\[/p' "${manifest}"); then
        echo "FAIL  ${crate} does not opt in to the workspace lints."
        echo "      Add to ${manifest}:"
        echo
        echo "          [lints]"
        echo "          workspace = true"
        echo
        fail=1
        continue
    fi
    echo "ok    ${crate} inherits the workspace lints"
done

# The forbid itself, in case somebody relaxes it centrally instead. `forbid`
# and not `deny`: deny can be lifted by an #[allow] anywhere inside a crate,
# which is exactly the hole this whole script exists to keep shut.
if ! grep -qE '^\s*unsafe_code\s*=\s*"forbid"' Cargo.toml; then
    echo "FAIL  the workspace no longer forbids unsafe_code."
    echo "      Cargo.toml [workspace.lints.rust] must say: unsafe_code = \"forbid\""
    fail=1
else
    echo "ok    the workspace forbids unsafe_code"
fi

# A belt-and-braces grep. The lint is the real check -- this only catches the
# case where someone has both dropped the opt-in and written the unsafe in the
# same change, so that the build is green and the loop above is the only thing
# standing between them.
if grep -rn --include='*.rs' -E '\bunsafe\b' crates/ src/ 2>/dev/null |
        grep -vE '(undocumented_unsafe_blocks|unsafe_code|//|/\*|\*)' | grep -q .; then
    echo
    echo "FAIL  the word 'unsafe' appears in source outside a comment:"
    grep -rn --include='*.rs' -E '\bunsafe\b' crates/ src/ 2>/dev/null |
        grep -vE '(undocumented_unsafe_blocks|unsafe_code|//|/\*|\*)' || true
    fail=1
fi

if [ "${fail}" -ne 0 ]; then
    echo
    echo "The unsafe quarantine is broken." >&2
    exit 1
fi

echo
echo "No unsafe anywhere in the workspace."
