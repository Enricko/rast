# Package icons (runtimes, apps and the catalog)

App Center shows an icon for every package instead of a two-letter monogram.
Official catalog runtimes ship the programming language/runtime's brand logo;
app-type packages can ship their own icon in the package manifest. Icons are
served through the signed catalog/manifest and rendered inside an `<img>`
element, never injected as inline markup.

## What users see

* Official runtime cards (Python, PHP, Go, Node.js, Rust, Java/Temurin, Bun,
  Deno, .NET, Clang/LLVM, Flutter, Composer, Zig, Kotlin, CMake, Gradle,
  Maven) show their brand icon.
* App-type packages installed from GitHub show the icon embedded in their
  `rast-package.json`.
* Packages without an icon keep the existing monogram fallback, so older
  catalogs and third-party packages continue to look right.
* The icon's optional `attribution` is used as the hover title.

The official icons are [Simple Icons](https://simpleicons.org) (CC0). Brand
names and logos are trademarks of their respective owners and are used only to
identify the corresponding runtime; the catalog README notes this as well.

## Manifest format

`icon` is optional on every package manifest (`rast-package.json`):

```json
{
  "icon": {
    "mediaType": "image/svg+xml",
    "data": "<standard base64 of the SVG, no data: prefix>",
    "attribution": "Optional: icon source/owner"
  }
}
```

Rules (enforced on import, install, catalog publication and catalog checks):

* `mediaType` is `image/svg+xml` or `image/png`.
* `data` is strict standard base64 (padding required, no whitespace, no
  `data:` prefix), decoding to 1 byte – 64 KiB.
* PNG files must start with the PNG signature.
* SVG must be valid UTF-8 and must not contain scripts or active content
  (`<script`, `<foreignObject`, `on…=` handlers, `javascript:`, external
  `href`/`xlink:href` URLs). RAST renders SVGs through `<img>`, which also
  blocks scripts and external references.
* `attribution`, when present, is at most 300 characters.

## Authoring: embed an icon

From the package repository (after generating `rast-package.json`):

```sh
rast package embed-icon icon.svg --manifest rast-package.json
rast package validate rast-package.json --dist dist
```

`embed-icon` accepts `.svg` or `.png`, applies the validation above, base64
encodes the file into the manifest and rewrites it as pretty JSON. The file
must be 64 KiB or smaller; a simple flat logo is usually a few KB.

For the official catalog the icons live in the `rast-packages` repository
(`icons/*.svg`) and are referenced from `catalog.config.json` per package:

```json
"python": {
  "name": "Python",
  "icon": "icons/python.svg",
  "...": "..."
}
```

The catalog tool embeds each file once at the catalog package level
(`packages[].icon`), so every version of that runtime shows the same icon
without duplicating it in each version manifest. Catalog signing covers the
embedded icon data like any other field.

## API

`GET /api/packages` returns `packages[].icon` as
`{"mediaType","data","attribution"?}` (omitted when the package has none).
Installed packages imported from GitHub use the icon from their newest
installed manifest; catalog packages use the catalog-level icon.
