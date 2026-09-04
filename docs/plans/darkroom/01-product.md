# Product: Darkroom

## Problem

"When I've culled a shoot down to the keepers, finishing them is the slow part. The
built-in Develop is too basic for a final look, so I round-trip to darktable or RapidRAW —
a different app, different shortcuts, and my versions and culling flow stay behind. And in
any editor, most of my time goes to fiddling a dozen sliders per photo when what I'm
actually doing is simpler than that: I know the better version when I *see* it, I just
can't type it in numbers."

## Success metric

**≥ 80% of edited keepers get their finished look inside ChairPhoto, without an external
round-trip.** Measured in-app per month: photos whose latest saved version was written by
the Darkroom vs. photos sent to an external editor (`Develop in…`). Today that number is
near zero — the built-in editor is used for crops, the looks happen elsewhere.

## Announcement — the blog post before the feature

ChairPhoto's new **Darkroom** develops photos the way you cull them: by choosing, not by
slider-fiddling. Open a keeper and the Darkroom deals you a **proof sheet** — your photo
developed a dozen ways, real renders, like test strips from a lab. Pick the one that's
closest, then refine it in **duels**: the Darkroom shows two prints, you tap the better
one, and each round sharpens the look — exposure, warmth, contrast — until it's yours.
Every experiment is a **version you can fork**, compare, and keep. Under the print sits
the **tone strip** — the histogram reborn as a control: eight tonal zones whose fill shows
where your photo's tones live, and dragging a zone lifts or crushes exactly that range.
The full slider kit (tone, film looks, LUTs, geometry) is always one panel away when you
want to steer by hand. And with a second monitor, the pop-out loupe becomes your **print**: a full-bleed
live render that updates as you choose, while your controls stay on the main screen.
Everything runs locally, and your original file is never touched.

## Screens

- `mockups/01-darkroom.html` — the Darkroom: print area (or "on the loupe screen"),
  keeper filmstrip, version shelf with forking, right rail with the classic slider kit
  collapsed into sections, and the two big actions: *Deal a proof sheet* / *Refine by duel*.
- `mockups/02-proof-sheet.html` — the proof sheet: the same photo developed 12 ways
  (auto-fix × your looks/presets/films), hover to compare against current, click to adopt
  as the working state (the current state is always one of the cells, so you can decline).
- `mockups/03-duel.html` — duel refinement: two full renders A/B, one key/tap picks the
  winner, a round strip shows what dimension each round explored (EV → warmth → contrast
  → looks), "keep both" forks a version, Esc exits with the current winner.
- `mockups/04-tone-strip.html` — the tone strip, our adjustable histogram: eight EV zones,
  fill height = pixel mass (the histogram never left), drag a zone up/down to lift/crush
  that range, drag the strip sideways for exposure, pinch for contrast. Lives under the
  print in the Darkroom (compact version visible in 01).
