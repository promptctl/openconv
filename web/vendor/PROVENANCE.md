# web/vendor

Third-party code, kept byte-identical to what its publisher served so that it can be
re-verified rather than trusted.

## livekit-client.js

| | |
|---|---|
| package | `livekit-client` |
| version | 2.22.1 |
| source | `https://cdn.jsdelivr.net/npm/livekit-client@2.22.1/+esm` |
| sha256 | `06c674dace8ef9cd94c09fd145da16e6bca3e4b8a17360d415134a079105c4a2` |
| retrieved | 2026-08-27 |

Vendored rather than imported from the CDN at page load, for two reasons. An ES module
`import` has no Subresource Integrity mechanism, so pinning the version in the URL
constrains *which* release is requested and not *which bytes* come back — and this page
holds an API key and an open microphone, so those bytes are worth constraining. And a
deployment whose network cannot reach jsdelivr would serve a page that loads and then
does nothing, which is the same failure `crates/openconv-server/src/web.rs` embeds the
rest of the page to avoid.

To re-verify, or to upgrade — change the version in one place, then check the digest:

```
curl -sfL https://cdn.jsdelivr.net/npm/livekit-client@2.22.1/+esm | shasum -a 256
```
