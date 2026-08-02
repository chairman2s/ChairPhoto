# Face-tagging test fixtures

## `quartet.jpg`

A group portrait with **four** clear, distinct frontal faces — used by the model-dependent
face-engine tests (`tests/faces_engine.rs`) to assert that detection finds ≥ 2 faces, that
embeddings are unit vectors, and that a same-face crop pair scores a higher cosine than a
different-face pair.

- **Source:** "Happy Faces barbershop quartet, 1973" — Seattle Municipal Archives, via
  Wikimedia Commons.
  <https://commons.wikimedia.org/wiki/File:Happy_Faces_barbershop_quartet,_1973_(50642025606).jpg>
- **Author / credit:** Seattle Municipal Archives.
- **License:** Creative Commons Attribution 2.0 (CC BY 2.0) —
  <https://creativecommons.org/licenses/by/2.0/>. Attribution: *"Happy Faces barbershop
  quartet, 1973" by Seattle Municipal Archives, CC BY 2.0.*
- **Modification:** downscaled from the 3000×3004 original to ≤ 1024 px (1023×1024) and
  re-encoded as JPEG (quality 88) to keep the repo light. No other edits.

The model-dependent tests **auto-skip** (with an `eprintln`) when the ONNX models are absent
and `CHAIRPHOTO_TEST_DOWNLOAD_MODELS` is unset, so the default `cargo test` suite is
offline-safe. Set `CHAIRPHOTO_TEST_DOWNLOAD_MODELS=1` to have the test download the models
and actually run the detection/embedding assertions.
