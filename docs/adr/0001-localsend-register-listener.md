# ADR 0001 — LocalSend discovery binds a short-lived inbound TCP port

- **Status**: Accepted
- **Date**: 2026-08-12
- **Issue**: [#55](https://github.com/chairman2s/chairphoto/issues/55)
- **Supersedes / relates to**: [#39](https://github.com/chairman2s/chairphoto/issues/39)
  (the discovery-socket fixes this builds on)

> First ADR in the repository. `docs/adr/` was proposed in #13's design discussion; #55 is the
> first change that actually warranted one, so it establishes the directory and the format.

## Context

LocalSend discovery is two-sided, and ChairPhoto was only implementing one side.

We announce over UDP multicast. A peer that hears the announcement replies with
`POST /api/localsend/v2/register` **over HTTP**, to the `port` the announcement advertised. Only
if that HTTP request fails does the peer fall back to a UDP `announce:false` datagram.

ChairPhoto advertised port 53317 and ran no TCP listener at all. That was a deliberate,
documented choice: with nothing listening, the peer's register POST fails, the peer falls back to
UDP, and our discovery socket hears it. It worked, and it kept ChairPhoto free of any inbound
port.

It stops working the moment another LocalSend-speaking process on the same machine holds 53317.
Then the peer's register POST **succeeds** — against that process — so the peer has no reason to
send the UDP fallback, and ChairPhoto never learns the peer exists.

This was observed live on 2026-08-12, not predicted: with the LocalSend desktop app running,
`ss -ltnp` showed it owning TCP 53317, the desktop app could see a phone on the LAN, and
ChairPhoto never could. The reverse case confirmed the mechanism — earlier the same day, with the
desktop app `--hidden` and TCP 53317 free, its own register POST to ChairPhoto was refused, it
fell back to UDP, and ChairPhoto discovered it correctly.

## Decision

For the duration of a discovery pass, ChairPhoto binds an **ephemeral** TCP port on `0.0.0.0` and
advertises that port in its announcement.

1. **Ephemeral, not 53317.** Binding the well-known port cannot work — in the failure case another
   process already holds it — and is not required. The announcement carries its own `port` field
   and peers POST to whatever it names. An OS-assigned port sidesteps the collision entirely, and
   both processes coexist.
2. **`register` only.** The listener answers `POST /api/localsend/v2/register` and refuses
   everything else. `prepare-upload` and `upload` (v2, and the v1 `send-request`/`send`) are
   refused with an explicit `403`, not left to fall through to a `404`, so the refusal reads as a
   decision rather than an omission.
3. **Bound only during a pass.** The listener lives inside the discovery future. When the pass's
   deadline fires, the future is dropped, taking the listener and every in-flight connection
   handler with it. Nothing of ChairPhoto's is LAN-reachable between discoveries.
4. **Hand-rolled minimal HTTP.** No server crate. This speaks exactly one request shape — a small
   JSON POST — and unused HTTP surface on a LAN-reachable port is precisely what we are trying not
   to have. Request size, body size, connection duration, and concurrent connections are all
   capped.
5. **Bind failure degrades, never fails.** If the listener cannot bind, discovery advertises its
   UDP port as before and relies on peers' UDP fallback. That is the pre-#55 behavior, which still
   works whenever nothing else holds the port.

## Consequences

**What this costs.** ChairPhoto now opens a LAN-reachable inbound port, which it never did before.
In a local-first, privacy-first app that is a real change and the reason this ADR exists rather
than a commit message. Mitigations are the scope limits above: ephemeral, short-lived,
single-route, size- and time-capped, auto-accepting nothing. Any LAN host can reach it while a
discovery pass is running and learn one thing — that ChairPhoto exists, and its alias and
fingerprint — which is exactly what the multicast announcement already broadcasts to the same
network. No catalog state is reachable, and no request can cause a write.

**What it buys.** Discovery works on a machine that also runs LocalSend, which is the common case
for anyone who uses both. It also makes the announcement honest: the advertised port is now a port
ChairPhoto genuinely serves on, rather than one it hoped would fail.

**The invariant that changed.** Before, the advertised port was the UDP discovery socket's port;
a test asserted the announcement's `port` equalled the datagram's real source port. That is
deliberately gone — the advertised port is now the TCP listener's, a different port by
construction. The replacement invariant is stronger and is what
`discover_advertises_a_port_it_actually_serves_register_on` pins: the advertised port must be one
ChairPhoto has really bound and really answers `register` on.

**What is still open.** ChairPhoto does not implement LocalSend's legacy subnet scan, so a peer
that neither receives multicast nor registers is still only reachable by manual IP. Tracked in
#39.

## Alternatives rejected

- **Bind TCP 53317.** The original diagnosis in `agent-notes/ready-for-review/issue-39-diagnosis.md`
  proposed this. It cannot work in the failure case it is meant to fix, since the collision *is*
  another process holding that port.
- **Keep the listener up for the app's lifetime.** Simpler, and would let peers register at any
  time rather than only during a pass. Rejected: a permanently open inbound port is a much larger
  commitment than a 5-second one, and discovery is user-triggered anyway, so the window when a
  reply is useful is exactly the window the pass is open.
- **Do nothing; document the limitation.** What #39 shipped. Rejected once the failure was
  observed in the field rather than merely predicted: "quit the LocalSend app before using
  ChairPhoto" is not a workaround a user should have to know.
