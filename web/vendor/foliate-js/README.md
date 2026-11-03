# foliate-js (vendored)

EPUB rendering engine for the Marginalia reader. Upstream: https://github.com/johnfactotum/foliate-js (MIT, see `LICENSE`).

- Pinned commit: `78914aef4466eb960965702401634c2cb348e9b1` (2026-05-01)
- Files are copied unmodified from that commit.

Included (what an EPUB needs): `view.js`, `epub.js`, `epubcfi.js`, `paginator.js`, `fixed-layout.js`, `overlayer.js`,
`progress.js`, `search.js`, `text-walker.js`, `footnotes.js`, and `vendor/zip.js` (zip.js by Gildas Lormeau, BSD-3-Clause,
the unzip layer).

Left out: `pdf.js` and `vendor/pdfjs/` (no PDF), `mobi.js`, `fb2.js`, `comic-book.js`, `vendor/fflate.js` (MOBI/KF8 only),
`tts.js`, `dict.js`, `opds.js`, `uri-template.js`, `quote-image.js`, `reader.js` and `ui/` (upstream's demo reader), the
build files. `view.js` still has dynamic `import()`s for the omitted formats; they only run when a MOBI, FB2, CBZ or PDF
is opened, which Marginalia never does (the server only stores EPUBs).

Loaded from `web/app.js` with `import('./vendor/foliate-js/view.js')`. To update: re-copy the same files from a newer
commit and change the commit above.
