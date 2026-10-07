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

## `regions/` — MWG face-region sidecars written by real tools (#154)

Read by the "real-tool sidecars" tests in `crates/core/src/xmp/regions/tests.rs`
(through `xmp::region_fixtures`). Each holds one face region, **Bob**, at stored-frame
center (0.325, 0.25), size 0.15 x 0.1, `AppliedToDimensions` 600 x 400, on a photo whose
EXIF Orientation is 6 — the values were chosen by hand (the tools write what they are
given and convert no frames); the serialisation is each tool's own, byte for byte.

Made on 2026-10-03 on copies of a scratch image, never on a user's file:

```bash
magick -size 600x400 xc:gray50 -fill white -draw "rectangle 150,80 240,120" P.jpg
exiftool -overwrite_original -Orientation#=6 -ExifImageWidth=600 -ExifImageHeight=400 P.jpg
```

- `exiftool-o6.xmp` — **exiftool 13.55** creating a sidecar from `P.jpg` (so it also
  copies the image's EXIF/TIFF tags into XMP, `tiff:Orientation` 6 included), one
  `rdf:Description` per namespace, single-quoted attributes, structs as
  `rdf:parseType='Resource'` with element-form fields:
  ```bash
  exiftool -o P.exiftool.xmp \
    '-XMP-mwg-rs:RegionInfo={AppliedToDimensions={W=600,H=400,Unit=pixel},RegionList=[{Area={X=0.325,Y=0.25,W=0.15,H=0.1,Unit=normalized},Name=Bob,Type=Face}]}' \
    -XMP-dc:Subject=Family P.jpg
  ```
- `exiv2-o6.xmp` — **exiv2 0.28.9** (its Adobe XMP Toolkit, `XMP Core 4.4.0-Exiv2`)
  adding the same region, field by field, and `tiff:Orientation` 6 to a one-keyword sidecar
  (`exiftool -XMP-dc:Subject=Family P.exiv2.xmp`), then re-serialising the whole packet:
  one `rdf:Description`, attribute-form simple fields, each region a nested
  `rdf:Description`:
  ```bash
  exiv2 -M"set Xmp.mwg-rs.Regions XmpText type=Struct" \
    -M"set Xmp.mwg-rs.Regions/mwg-rs:AppliedToDimensions XmpText type=Struct" \
    -M"set Xmp.mwg-rs.Regions/mwg-rs:AppliedToDimensions/stDim:w 600" …   # every field
    -M"set Xmp.tiff.Orientation 6" P.exiv2.xmp
  ```

**Not captured:** digiKam, Lightroom and Picasa are not installed on this machine (checked
2026-10-03: no `digikam` on `PATH`; Lightroom and Picasa do not run on Linux), so no
sidecar here is theirs, and nothing here shows which frame those tools write regions in on
a rotated photo. digiKam writes XMP through Exiv2, so `exiv2-o6.xmp` has the serialisation
library's shape, not digiKam's choice of fields. darktable 5.6.1 is installed and its
library carries MWG region strings (`Xmp.mwg-rs.Regions/mwg-rs:RegionList[`), but its
`darktable-cli` takes no face regions to write, so no darktable sidecar was captured either.
