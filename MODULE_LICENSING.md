# ChairPhoto — Licensing for module authors

ChairPhoto is licensed **GPL-3.0**. This note explains what that means if you are
writing a module, and in particular what it does *not* restrict.

## The short version

- **Your module must be GPL-3.0** (or a GPL-compatible license) if you distribute it.
  Modules are Rust compiled into the ChairPhoto binary through the `Module` trait, so
  distributing one means distributing a fork or patch of ChairPhoto itself — a
  derivative work.
- **The external service your module talks to does not have to be open-source.**
  A module may call any web API — free, paid, proprietary, closed. The service runs on
  someone else's machine as a separate program; the GPL does not reach across a network
  boundary.
- **Charging money is fine.** You may sell a service your module depends on, require an
  API key, meter usage, or run a subscription. The GPL restricts how *code* is
  distributed, not whether you can be paid.

## Why it's drawn this way

The project's intent is that every *version of ChairPhoto* stays open — forks,
modifications and modules included — while leaving room for a real service economy
around it. Those two goals are compatible precisely because the code/service line is
also the GPL's line.

This is not hypothetical: the bundled Flickr, SmugMug, Instagram and AI-tagging modules
already work this way. They are part of the GPL codebase and they call proprietary
third-party services (the Flickr API, the Claude API) that nobody expects to be open.

## What the GPL does *not* do here

- **It does not force you to publish a private module.** The GPL triggers on
  *distribution*. If you write a module for yourself and never share it, you owe
  nobody anything — including source.
- **It does not make your service's source part of ChairPhoto.** Server-side code you
  run is yours.
- **It does not stop commercial use.** Selling ChairPhoto, or a fork of it, is allowed;
  the requirement is that recipients get the source under the same license.

## Practical checklist

1. Ship a `LICENSE` (GPL-3.0) with your fork or patch.
2. State clearly which external service the module requires, and whether it costs
   money — users should know before building or installing it.
3. There is no separate module version to track: a compiled-in module ships at
   whatever ChairPhoto version it was built into (see `docs/plugin-system.md`).
4. Never bundle credentials. Take API keys from the user at runtime, the way the
   built-in publishing modules do.

## Bundled third-party code

ChairPhoto vendors **LibRaw** (`crates/core/vendor/LibRaw`, a pinned git submodule) and
compiles it into the binary for RAW decoding. LibRaw is dual-licensed, LGPL-2.1 or CDDL-1.0
(`COPYRIGHT` in that tree). ChairPhoto takes it under the **LGPL-2.1** option
(`LICENSE.LGPL`): section 3 of that license lets a copy be distributed under the GNU GPL,
version 2 or any later version, which is how it sits inside a GPL-3.0-only program. The
CDDL option is not used — the FSF lists CDDL as incompatible with the GPL. (Corrected
2026-09-24; this note had named the CDDL option. A reading of the licenses, not legal
advice.) LibRaw's own sources are unmodified; a decoder bump is a submodule pointer
change. Its notices travel with every binary: the package installs LibRaw's `COPYRIGHT`
and `LICENSE.LGPL` next to ChairPhoto's own license (`packaging/PKGBUILD`), and LibRaw's
bundled parts (dcraw, DCB/FBDD, X3F, Adobe DNG SDK pieces) are covered by that
`COPYRIGHT` file.

The lens-correction reader (`crates/core/src/lens/`) is ported from **RAWmakase**
(<https://github.com/pch/rawmakase>, commit `80b6433`), Copyright (c) 2026 RAWmakase
contributors, under the MIT License, which permits use in a GPL-3.0 program provided the
notice is kept: it is in `crates/core/src/lens/LICENSE-RAWmakase`, which the package
installs next to ChairPhoto's own license (`packaging/PKGBUILD`), and quoted below. Only RAWmakase's own MIT code is taken — none of its files under the
Adobe DNG SDK license, and none of its tables measured from Adobe Camera Raw.

> Permission is hereby granted, free of charge, to any person obtaining a copy of this
> software and associated documentation files (the "Software"), to deal in the Software
> without restriction, including without limitation the rights to use, copy, modify,
> merge, publish, distribute, sublicense, and/or sell copies of the Software, and to
> permit persons to whom the Software is furnished to do so, subject to the following
> conditions:
>
> The above copyright notice and this permission notice shall be included in all copies
> or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED,
> INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
> PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
> HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF
> CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR
> THE USE OR OTHER DEALINGS IN THE SOFTWARE.

The GPUI app (`crates/app`) embeds three more sets of third-party assets: two found while
fixing #173 (#177), and the third (gpui-kit-assets' own 6 non-Lucide SVGs) found by a
post-merge review of #177 itself:

- **Lucide** icon SVGs (<https://lucide.dev>), ISC License, Copyright (c) 2026 Lucide Icons
  and Contributors — the icons the app's own code names (`IconName::…`) that gpui-kit's
  default bundle does not carry (`crates/app/src/assets.rs`'s `ExtraIcons`). They are embedded
  from the `gpui-kit-assets` crate's own copy of the Lucide set — the same source gpui-kit's
  default bundle and every icon gpui-component itself draws already use — so no new upstream
  dependency is introduced, only more icons drawn from it. A subset of Lucide's icons are
  themselves derived from the Feather project and carry an additional MIT notice (Copyright
  (c) 2013-present Cole Bemis); both notices are one file, `LICENSE-LUCIDE`, which this repo
  keeps its own copy of at `crates/app/assets/icons/LICENSE-LUCIDE` and the package installs
  next to ChairPhoto's own license (`packaging/PKGBUILD`).
- **Instrument Sans** and **Instrument Serif** (<https://github.com/Instrument/instrument-sans>),
  Copyright 2022 The Instrument Sans Project Authors, under the **SIL Open Font License 1.1**
  with no Reserved Font Name — the app's UI and display typefaces, embedded as TTF
  (`crates/app/assets/fonts/`, `crates/app/src/assets.rs`). The licence texts sit beside the
  font files (`OFL-InstrumentSans.txt`, `OFL-InstrumentSerif.txt`) and the package installs
  both next to ChairPhoto's own license (`packaging/PKGBUILD`).
- **gpui-kit-assets' own icons.** That crate's default icon bundle (`default-icons.txt`,
  what every icon gpui-component itself draws from unless `ExtraIcons` above overrides it)
  embeds 104 SVGs in total; 98 are the Lucide set just covered. The other 6 — `github.svg`
  (the GitHub mark), `window-close.svg`, `window-maximize.svg`, `window-minimize.svg`,
  `window-restore.svg`, and `resize-corner.svg` — are **not** Lucide icons. They're the
  crate's own original work, under its own **Apache License 2.0** (its `Cargo.toml`:
  `license = "Apache-2.0"`). The crate ships no `LICENSE-APACHE` file of its own (unlike
  `LICENSE-LUCIDE`), so this repo keeps a verbatim copy of the plain upstream Apache License
  2.0 text at `crates/app/assets/icons/LICENSE-APACHE-GPUI-KIT-ASSETS`, and the package
  installs it next to ChairPhoto's own license (`packaging/PKGBUILD`). An earlier version of
  this note (#177) said gpui-kit and friends "ship no assets of their own" — wrong: these 6
  SVGs are exactly that, just not Lucide's.

Everything else `crates/app/assets` and the gpui-component/gpui-base/gpui-kit crates bring in
is Rust source code pulled in as an ordinary Cargo dependency (Apache-2.0, per their own
`LICENSE-APACHE`), not a bundled asset — the same category as the project's other Rust
dependencies, covered next.

## Third-party Rust crate notices (#244)

`chairphoto-app`'s own dependency graph — everything `Cargo.lock` pulls in for the package as
shipped (default features, plus the `flickr`/`smugmug` opt-ins `packaging/PKGBUILD` enables;
`crates/core` and `crates/model` are path dependencies of it, so their graphs are included,
not scanned separately) — is a little over 600 crates, almost all MIT and/or Apache-2.0, with
a handful of BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, CC0-1.0, 0BSD, MPL-2.0, bzip2-1.0.6 and
CDLA-Permissive-2.0 crates mixed in. None of those licenses require a packaged notice file the
way GPL/LGPL attribution does, but `THIRD_PARTY_LICENSES.txt` collects them anyway, deduplicated
by exact license text, and the package installs it next to this file's other license texts
(`packaging/PKGBUILD`).

It is generated with [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) from
`about.toml` (the accepted-license allowlist, with `targets` pinned to
`x86_64-unknown-linux-gnu` so a platform-specific dependency this package never actually links —
one surfaced only once that pin was lifted to check: `libfuzzer-sys`, under the unreviewed
`NCSA` license — never has to be vetted) and `about.hbs` (the plain-text template). It is
committed, not generated during `packaging/PKGBUILD`'s `build()`, so a `makepkg` build stays
offline and needs neither `cargo-about` nor network access; CI instead checks on every push
that the committed file still matches what `about.toml`/`about.hbs`/`Cargo.lock` would
generate today (`.github/workflows/ci.yml`), so it cannot silently go stale. See
`packaging/README.md` ("Third-party notices") for the regeneration command and what to do when
a dependency change introduces a license `about.toml` doesn't already accept.

MPL-2.0 (`option-ext`) is the one weak-copyleft license in the graph: its share-alike
obligation attaches only to the MPL-covered file itself, not to code merely linked against it,
so it does not reach into GPL-3.0-only ChairPhoto's own terms. CDLA-Permissive-2.0
(`webpki-roots`) licenses a data bundle (Mozilla's CA root certificates), not code, and is
itself permissive. Both are deliberate entries in `about.toml`'s allowlist, not oversights.
