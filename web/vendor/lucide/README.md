# Selected Lucide SVGs

Upstream: https://github.com/lucide-icons/lucide, release **1.61.0**, commit
`c29c276f16590cf017126671535ad8c792be64d8`.
Official usage/license: https://lucide.dev/guide/lucide and https://lucide.dev/license.

Only the SVGs the client uses are included: its controls plus a curated set of business icons that
app menus pick from (152 in all). `UPSTREAM.toml` pins every pristine
SVG and the complete upstream LICENSE by SHA-256. The existing vendor checker
retrieves those exact commit URLs and verifies bytes; no full icon package is shipped.
The license includes Lucide ISC and the applicable Feather MIT attribution.

`web/scripts/generate-lucide.py` converts this local subset to `nodes.ts`; it allows
only geometry tags/attributes, plus `fill="currentColor"` on a dot. The client creates SVG elements from this fixed data,
never parses metadata as markup, fetches an icon, or loads a font/framework/CDN.
The complete license is retained in a legal comment in nodes.ts for bundled output.
The client icon tests compare emitted geometry with these raw SVGs.

Menu metadata uses `lucide:<name>` (for example `lucide:settings`, `lucide:library`); null
retains a generic layout-grid icon, independent of translated menu captions. A name this
subset does not ship shows the same default icon and one console warning per value, so an
addon never stops the client from starting; a client test fails for a shipped addon that
names an icon outside the subset. The Technical › Menus form picks from this subset
(`web.menu_icon` widget). Existing image menu icons remain supported by their pre-existing path.
SVGs are decorative, inherit currentColor, are not focusable, and retain the parent
button's text/ARIA label and interaction. New/Save/Discard/Reload/Delete, pager,
view switch, navigation and search use the same subset. No speed claim or benchmark.
