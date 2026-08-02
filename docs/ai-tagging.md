---
title: "AI Tagging"
description: "Vision-model and CLIP-based tag suggestions, and how a suggestion becomes a tag."
tags:
  - chairphoto/module
  - chairphoto/tagging
aliases:
  - "Smart Tagging"
  - "AI suggestions"
---

# AI Tagging

ChairPhoto proposes tags for a photo two different ways. Both are optional, both default to
running on your machine, and both are **non-destructive**: they produce suggestions you accept
or reject, never tags applied behind your back. Rejections are remembered and not re-proposed.

| Engine | Plugin | How it decides |
|---|---|---|
| **AI Tagging** | `ai` | A vision-language model reads the image and your tag vocabulary and picks tags. Also proposes genuinely new tags. |
| **Smart Tagging** | `smarttags` | A local CLIP index learns from photos you have already tagged and suggests by visual similarity. No language model. |

They are separate Cargo features and separate tables, because they are different technology and
independently useful. Smart Tagging gets better the more you tag; AI Tagging works from the
first photo.

## Privacy

The default provider is local. **Images are never sent to a cloud provider without an explicit
opt-in**, and a bulk cloud run shows a "N images → ~$X" confirmation first.

A tag can be marked **private** (`tags.private`) — typically every name under `People`. Private
tags are stripped from the vocabulary sent to cloud providers, so personal names never leave the
machine; the local model still receives the full list. Set through the tag context menu
("Make private…", recursive on a parent) via `set_tag_private`. Filtering happens in
`ai::taxonomy_text`, gated on `Config::is_local`. Privacy affects only what is transmitted — not
export, filtering, or display.

## AI Tagging (`ai` plugin)

### Providers

One provider is active at a time, chosen in settings:

- **Ollama** (local, default) — POSTs the image to `/api/chat` with `format: "json"`.
  Default model `llava:latest`, chosen because it fits a 10 GB GPU (~4.9 GB) and answers in
  roughly 14 s. Larger vision encoders such as qwen2.5-VL need 10–13 GB and silently fall back
  to CPU, where they take minutes per image; prefer them only on a bigger card. `moondream`
  (~1.8 GB) is a lighter option.
- **Claude**, **OpenAI**, **Gemini** — cloud, each adapting the same request to its own wire
  format and image content block.

Images are downsized to roughly 1024 px on the long edge before sending: enough for recognition,
small enough to keep local latency and cloud token cost down.

### Vocabulary and output

Each tag is presented to the model as its full path plus description and synonyms:

```
Transportation/Watercraft/Ferry — a vessel carrying passengers/vehicles across water
  (synonyms: ferryboat, car ferry)
```

The model returns one `tags` array; ChairPhoto — not the model — decides what is new by
resolving each path against the live vocabulary:

```jsonc
{ "tags": [ { "path": "Animals/Birds/Gull", "confidence": 0.0,
              "reason": "short justification",
              "description": "used only if this tag turns out to be new",
              "synonyms": ["optional, for new tags"] } ] }
```

A known path becomes an existing-tag suggestion. An unknown path becomes a new-tag proposal, and
on accept the tag is created *with* the proposed description and synonyms, so a new tag arrives
already documented.

**New tags nest by is-a, never by loose fit.** A tag goes under an existing branch only when it
is a narrower *kind* of that branch — `Transportation/Watercraft/Kayak` is valid because a kayak
is a watercraft. When no branch is a genuine broader category the model must start a new one:
a sunset is not a kind of public place, so `Nature/Sunset`, never `Public place/Sunset`. The
prompt carries that counterexample because earlier "extend an existing branch" wording pushed
small local models into mis-nesting. Covered by tests in `plugins::ai::tests`.

Valid JSON is enforced with structured output where the provider supports it (Ollama's `format`
schema, Claude's tool output) and a lenient parser as a backstop — it extracts the outermost
JSON object and defaults missing fields.

### Suggestion storage

The plugin owns `ai__suggestions` (`photo_id`, `path`, `state`, `confidence`, `reason`,
`created_at`); core defines no plugin tables. Accepting assigns the tag, creating it first if it
was a new proposal. Rejecting marks the row `rejected`, which both filters it from future results
and feeds it back into the prompt as "previously rejected here", so re-runs do not repeat it.

### Burst grouping and representative dispatch

A culling session produces bursts — 100 frames may be 10 distinct subjects — and tagging every
near-identical frame wastes cloud spend and local time. Instead the selection is clustered, only
a representative frame from each cluster is sent, and the results propagate to the rest of the
cluster reviewably.

Clustering runs cheapest-first, each tier refining the last:

1. **Capture time.** A new group starts when the gap to the previous photo exceeds
   `ai.burst_time_gap_secs` (default `15`). Catches classic bursts but over-groups — five
   seconds apart can be a 180° turn to a new subject.
2. **Perceptual hash.** A 64-bit hash computed from the cached thumbnails (`index_phashes`,
   milliseconds per photo, no extra runtime). Within a time group, frames within
   `ai.burst_hamming_threshold` Hamming distance (default `10`) are the same scene; a jump splits
   the group. The hash lives in core rather than the plugin because near-duplicate detection and
   auto-stacking reuse it.

The representative is the highest-rated frame where you have already expressed a preference,
otherwise the sharpest by Laplacian-variance blur score.

Suggestions returned for the representative are stored for **every** cluster member as `pending`,
carrying provenance (`source_photo_id`) and slightly reduced confidence. The review UI shows the
group — "34 photos tagged from DSC01234" — and offers accept-for-group with per-photo reject; any
member can be re-run individually, which supersedes its propagated suggestions. Propagation is
never silent auto-apply: it trades a little recall for a large cost cut, so it has to stay
reviewable.

## Smart Tagging (`smarttags` plugin)

Learns from the photos you have already tagged. Fully local, no language model.

### Model

**Xenova CLIP ViT-B/32 vision tower** (Apache-2.0 ONNX export of `Xenova/clip-vit-base-patch32`),
about 350 MB. Input `[1, 3, 224, 224]` normalized `pixel_values`; output a `[1, 512]` embedding,
returned L2-normalized so cosine similarity is a plain dot product.

The model is **sha256-pinned**, verified both at download and at load. Download is on demand,
never automatic, emits `smarttags:download_progress {done, total}`, and finishes with an atomic
rename so a crash mid-download cannot leave a truncated file. `smarttags.model_path` points at a
user-supplied model; blank or absent falls back to the pinned default under
`<app_data_dir>/models/smarttags/`.

### Pipeline

| Module | Responsibility |
|---|---|
| `models.rs` | Resolve model path, download with SHA-256 verification, report `ModelStatus` to the UI |
| `embed.rs` | `encode_jpeg` — decode, resize to 224×224, CLIP-normalize, run ONNX, L2-normalize. Session pool with CUDA execution provider and CPU fallback |
| `store.rs` | `smarttags__embeddings` (photo_id PK, f32-LE vector BLOB, indexed_at); lazy schema, resume queue by LEFT JOIN |
| `indexer.rs` | Resumable background index: parallel preview-load and embed, sequential upsert, `smarttags:progress {done, total}`, abort-safe |
| `suggest.rs` | kNN suggestion engine and the `smarttags__suggestions` state machine |
| `classifier.rs` | Per-tag logistic classifiers in `smarttags__classifiers` |

### How a suggestion is produced

1. Load the query photo's embedding.
2. Brute-force cosine scan over all rows — comfortable to roughly 50 k photos.
3. Keep the top 20 neighbours scoring at least 0.60.
4. Score each candidate tag as the sum of similarities across neighbours carrying it.
5. Normalize to [0, 1] and blend with that tag's classifier where one exists.
6. Drop implied ancestors — suggesting `Animals/Birds/Gull` drops `Animals/Birds` and `Animals`.
7. Skip tags already on the photo and any path previously rejected.
8. Upsert survivors into `smarttags__suggestions`.

### Per-tag classifiers

Any tag with at least `smarttags.min_train_samples` (default 10) confirmed, embedded photos gets
a binary logistic-regression classifier: full-batch gradient descent, 200 epochs, learning rate
0.1, L2 λ=1e-4 — well under a second to train for typical catalogs. Weights, bias and sample
count persist in `smarttags__classifiers`.

Blending is `(knn_score + cls_weight × cls_score) / (1 + cls_weight)` with
`cls_weight = (sample_count / 50).clamp(0, 1)`. At 10 samples the classifier contributes about
17% of the final score, at 25 about 33%, and from 50 it is an even split with kNN — so a
classifier earns influence as evidence accumulates.

`smarttags_train_classifiers` compares `trained_at` against the newest accepted or rejected
suggestion and rebuilds only stale classifiers.

The embedding index is reusable infrastructure — near-duplicate detection and similarity-based
albums are natural consumers. Full VLM fine-tuning is out of scope: it needs a real training
pipeline and far more labels than a personal catalog provides.

## Settings

Stored in the catalog's `settings` table.

| Key | Purpose |
|---|---|
| `ai.provider` | `ollama` (default), `claude`, `openai`, `gemini` |
| `ai.ollama_url`, `ai.ollama_model` | Local endpoint and model |
| `ai.cloud_model`, `ai.cloud_api_key` | Claude |
| `ai.openai_model`, `ai.openai_api_key` | OpenAI |
| `ai.gemini_model`, `ai.gemini_api_key` | Gemini |
| `ai.prompt_template` | Optional custom prompt |
| `ai.existing_only` | Suggest only tags already in the vocabulary; drop new proposals |
| `ai.min_confidence` | Hide suggestions below this score |
| `ai.burst_time_gap_secs` | Burst clustering gap, default `15` |
| `ai.burst_hamming_threshold` | Burst hash similarity, default `10` |
| `smarttags.model_path` | Override the pinned CLIP model |
| `smarttags.min_train_samples` | Minimum confirmed photos before training a classifier, default `10` |

## Cargo features

- `ai` — the vision-language path. Pulls in `reqwest`.
- `smarttags` — CLIP inference, CPU only.
- `smarttags-cuda` — additive; enables the `ort/cuda` execution provider. Needs the CUDA runtime
  and cuDNN 9 on the loader path; a missing stack logs a warning and falls back to CPU.
- `--no-default-features` — no ONNX or CLIP code is compiled and the build stays green.
