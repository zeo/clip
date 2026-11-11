# Third-party notices

clip's own code is dual-licensed under MIT or Apache-2.0. `clip.exe` also carries the work of others, listed here.

## Fonts and sound

| Component | License | Source |
|---|---|---|
| Source Sans 3, regular and medium | OFL-1.1 | https://github.com/adobe-fonts/source-sans |
| Commit Mono V143 | OFL-1.1 | https://github.com/eigilnikolajsen/commit-mono |
| `assets/saved.wav`, `screenshot.wav` from capscr, unchanged | MIT, copyright (c) 2024-2025 capscr contributors | https://github.com/zeo/capscr |

The font license texts are in `web/public/fonts`.

## Code built in

| Component | License | Source |
|---|---|---|
| Tauri and its plugins | MIT or Apache-2.0 | https://github.com/tauri-apps/tauri |
| windows-rs | MIT or Apache-2.0 | https://github.com/microsoft/windows-rs |
| hdrkit shaders | MIT or Apache-2.0 | https://github.com/zeo/hdrkit |
| Svelte | MIT | https://github.com/sveltejs/svelte |
| serde, serde_json | MIT or Apache-2.0 | https://github.com/serde-rs |

The other Rust crates are listed with their versions in `Cargo.lock`. Most are MIT, Apache-2.0 or both. The rest are under Unicode-3.0 (the ICU4X crates), BSD-3-Clause (brotli), Zlib, Unlicense, 0BSD or CC0-1.0, and five are MPL-2.0: cssparser, cssparser-macros, selectors, dtoa-short and option-ext. clip uses those MPL-2.0 crates unmodified, and their source is on crates.io.
