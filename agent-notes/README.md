# agent-notes

Working notes agents leave for each other: review findings, hand-offs between sessions, notes
from one tool that another will read.

**Only this README is tracked.** Everything else here is ignored by git (see the root
`.gitignore`) and stays on the machine it was written on.

## Why the directory exists

These notes have to live somewhere. Without a named home they end up in the repository root,
where `git add -A` sweeps them into an unrelated commit — which is exactly how a PR review
file was committed to a feature branch and then described as untracked for several turns,
because nobody ran `git ls-files` to check.

Ignoring them by name after the fact only fixes the file you remembered. A directory fixes the
class.

## What belongs here

- Review findings and their resolution status.
- Hand-off notes between sessions, or between different agents working the same change.
- Investigation scratch worth keeping for a few days but not worth keeping forever.

## What does not

- Anything a future reader of the repository would need. That is a design document under
  `docs/`, a comment next to the code it explains, or a commit message.
- Anything that should outlive the work in progress. This directory is deliberately local:
  it is not backed up, not shared by cloning, and not reviewed.

## Conventions

- One file per topic, named for what it covers — `review-pr6-findings.md`, not `notes.md`.
- Say which commit or PR a note describes, and at which revision it was written. A finding
  without a revision cannot be checked later.
- Record what was declined and why, not only what was fixed. The reasoning behind a decision
  not to act is the part that is hardest to reconstruct.
- Delete a note when its work has landed. This is scratch space, not an archive.
