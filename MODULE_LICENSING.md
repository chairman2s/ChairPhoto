# ChairPhoto — Licensing for module authors

ChairPhoto is licensed **GPL-3.0**. This note explains what that means if you are
writing a module, and in particular what it does *not* restrict.

## The short version

- **Your module must be GPL-3.0** (or a GPL-compatible license) if you distribute it.
  Modules load through the host API and run inside ChairPhoto, so a distributed module
  is a derivative work.
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

1. Ship a `LICENSE` (GPL-3.0) with your module.
2. State clearly in your README which external service the module requires, and whether
   it costs money — users should know before installing.
3. Set `minHostVersion` to the oldest ChairPhoto release whose host API you use
   (see the host-API stability contract in the plugin system docs).
4. Never bundle credentials. Take API keys from the user at runtime, the way the
   built-in publishing modules do.
