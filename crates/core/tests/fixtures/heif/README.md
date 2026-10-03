# HEIF orientation fixtures (#154)

Tiny HEIC files that carry their turn in the container (`irot`/`imir`), in EXIF
Orientation, in both, or in neither. Read by `metadata::heif` tests
(`real_heif_files_give_their_container_turn`, `container_turn_is_the_previews`) and the
face-region frame tests in `plugins/faces/regions.rs`.

**Provenance.** Made on 2026-10-03 with real tools, no hand editing:

```bash
magick -size 60x40 xc:black -fill white -draw "rectangle 10,5 14,9" base.png   # ImageMagick 7.1.2-31
heif-enc -q 90 base.png -o plain.heic                                            # libheif 1.23.4, x265
heif-enc -q 90 --rotate-cw 90 base.png -o rot90.heic
heif-enc -q 90 --flip-h base.png -o fliph.heic
heif-enc -q 90 --flip-v base.png -o flipv.heic
heif-enc -q 90 --rotate-cw 90 --flip-h base.png -o rot90fliph.heic
exiftool -Orientation#=6 -o rot90_e6.heic rot90.heic                              # exiftool 13.55
exiftool -Orientation#=1 -o rot90_e1.heic rot90.heic
exiftool -Orientation#=6 -o plain_e6.heic plain.heic
```

`heif-enc` keeps the pixels as given and records the turn in the container: the coded
image is always the 60x40 picture (padded to 64x64 and cropped back by a `clap` listed
before the turn), and

| File | Container (`heif-info`) | EXIF Orientation | Turn as EXIF code |
|---|---|---|---|
| `plain.heic` | none | none | 1 |
| `rot90.heic` | `irot` angle 3 (270° anticlockwise) | none | 6 |
| `fliph.heic` | `imir` 1 (left–right) | none | 2 |
| `flipv.heic` | `imir` 0 (top–bottom) | none | 4 |
| `rot90fliph.heic` | `irot` 3, then `imir` 1 | none | 5 |
| `rot90_e6.heic` | `irot` 3 | 6 (agrees) | 6 |
| `rot90_e1.heic` | `irot` 3 | 1 (disagrees) | 6 |
| `plain_e6.heic` | none | 6 (disagrees) | 1 |

Decoded by `heif-convert` and by `magick` with and without `-auto-orient` (the preview
path), every file showed the white block where the container's turn puts it and nowhere
the EXIF Orientation would: ImageMagick's HEIC decoding applies `irot`/`imir` once and
ignores EXIF Orientation. These are not camera files: no iPhone HEIC was available on this
machine, so Apple's grid layout (the turn on a `grid` primary item) is covered only by the
hand-made byte buffers in `metadata/heif.rs`.
