# Vendored third-party files

## mermaid.min.js

- Package: `mermaid` 11.17.2 (the newest 11.x; the npm `latest` tag is already 12.x)
- File: `dist/mermaid.min.js` from the npm tarball
- Source: https://registry.npmjs.org/mermaid/-/mermaid-11.17.2.tgz
- Tarball integrity (npm `dist.integrity`, verified before extracting):
  `sha512-V6K3C8EBdEsPFZXSKMJe6ppQOENxuHARr9GvHX4hh47lAbhMRD9qf4oEK7LoaRQxULMa80/qt5gHO73aCleBBg==`
- License: MIT

The file is embedded in the binary and served at `/vendor/mermaid.min.js`; the page loads it only when an article
contains a `mermaid` code block. To update, fetch the new tarball, check its sha512 against the registry's
`dist.integrity`, replace the file and edit this note.
