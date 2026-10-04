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
