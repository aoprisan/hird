#!/usr/bin/env bash
#
# Check the static site in docs/ for links that go nowhere.
#
# Two failure modes matter for a hand-written site: an `href="#section"` whose
# id was renamed, and a stylesheet or page that is not in the published
# artifact. Both are silent in a browser, so they are checked here instead.
#
#   .github/scripts/check-docs-links.sh   (or: just docs-check)

set -uo pipefail

cd "$(dirname "$0")/../../docs"

status=0
for file in *.html; do
    ids=$(grep -o 'id="[^"]*"' "$file" | cut -d'"' -f2 | sort -u)
    for anchor in $(grep -o 'href="#[^"]*"' "$file" | cut -d'#' -f2 | tr -d '"' | sort -u); do
        if ! grep -qxF -- "$anchor" <<<"$ids"; then
            echo "$file: href=\"#$anchor\" has no matching id"
            status=1
        fi
    done
    # Local references — anything without a scheme or a fragment — must exist.
    for asset in $(grep -oE '(href|src)="[^"#:]+"' "$file" | cut -d'"' -f2 | sort -u); do
        if [[ ! -e "$asset" ]]; then
            echo "$file: references missing file $asset"
            status=1
        fi
    done
done

if [[ $status -eq 0 ]]; then
    echo "docs: internal links and assets resolve"
fi
exit $status
