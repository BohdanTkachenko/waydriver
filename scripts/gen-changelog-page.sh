#!/usr/bin/env bash
# Generate docs/src/changelog.md from the canonical crate CHANGELOG.
#
# release-plz writes version headings as `## [0.3.3](compare-url) - DATE`. In
# mdBook that becomes an <a> nested inside the heading's own <a class="header">,
# which is invalid HTML — the sidebar/page TOC can't render it, so the changelog
# nav collapses to a meaningless repeating "Added / Fixed / Other" list with no
# versions. Stripping the Markdown link from heading lines makes every version a
# clean, navigable heading while leaving in-body links (e.g. PR refs) intact.
set -euo pipefail

src="crates/waydriver/CHANGELOG.md"
out="docs/src/changelog.md"

{
  echo "<!-- Generated from ${src} by scripts/gen-changelog-page.sh — do not edit by hand. -->"
  echo
  # On heading lines only, replace [text](url) with text.
  sed -E '/^#{1,6}[[:space:]]/ s/\[([^][]+)\]\([^()]+\)/\1/g' "$src"
} > "$out"

echo "Wrote ${out}"
