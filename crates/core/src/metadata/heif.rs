//! A HEIF image's own turn (#154): the `irot` and `imir` transformative item properties of its
//! primary item (ISO/IEC 23008-12), as an EXIF Orientation code.
//!
//! A HEIF reader turns the decoded image by these container properties, not by the EXIF
//! Orientation the file may also carry. ChairPhoto's HEIC preview is ImageMagick's libheif
//! delegate, which applies them and then leaves EXIF Orientation unapplied (observed with
//! ImageMagick 7.1.2-31 and libheif 1.23.4; `container_turn_is_the_previews` pins it where
//! `magick` can decode HEIC). The face-region frame (`xmp::RegionFrame::with_container`)
//! takes the display frame's turn from here.
//!
//! Only the box structure is read — `ftyp`, then the top-level `meta` box with its `pitm`,
//! `iprp`/`ipco` and `ipma` — never the image data. Anything this reader cannot follow is an
//! error, never "no turn": the caller refuses rather than guess.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The largest `meta` box read. An iPhone's is a few kilobytes (its tile grid's `iloc`); the
/// image data lives in `mdat`, which is skipped.
const MAX_META: u64 = 16 * 1024 * 1024;

/// Whether ChairPhoto previews `path` as a HEIF, through ImageMagick's libheif delegate — the
/// files whose display frame is their container's turn. The rule `thumbnails` decodes by.
pub fn is_heif(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("heic" | "heif")
    )
}

/// The turn a HEIF's container gives its primary image, as the EXIF Orientation code (1-8)
/// that turns the stored image the same way: 1 when it has no `irot`/`imir`. An error when the
/// file is not ISOBMFF, has no readable `meta` box, primary item or property associations, or
/// has a `clap` (clean aperture) after a turn, whose crop of the stored frame this reader does
/// not follow.
pub fn container_orientation(path: &Path) -> Result<u8, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = read_meta(&mut file).map_err(|e| format!("{}: {e}", path.display()))?;
    orientation_in_meta(&meta).map_err(|e| format!("{}: {e}", path.display()))
}

/// [`container_orientation`] over a whole file's bytes.
#[cfg(test)]
pub(crate) fn orientation_of(file: &[u8]) -> Result<u8, String> {
    orientation_in_meta(&read_meta(&mut std::io::Cursor::new(file))?)
}

/// The content of the top-level `meta` box, after checking the file starts with `ftyp`.
fn read_meta<R: Read + Seek>(r: &mut R) -> Result<Vec<u8>, String> {
    let io = |e: std::io::Error| e.to_string();
    let len = r.seek(SeekFrom::End(0)).map_err(io)?;
    let mut pos = 0u64;
    while pos < len {
        r.seek(SeekFrom::Start(pos)).map_err(io)?;
        let (kind, header, size) = box_header(r, len - pos)?;
        if pos == 0 && &kind != b"ftyp" {
            return Err("not an ISOBMFF file: it does not start with an ftyp box".into());
        }
        if &kind == b"meta" {
            let content = size - header;
            if content > MAX_META {
                return Err(format!("its meta box is {content} bytes, more than this reader reads"));
            }
            let mut meta = vec![0; content as usize];
            r.read_exact(&mut meta).map_err(io)?;
            return Ok(meta);
        }
        pos += size;
    }
    Err("it has no top-level meta box".into())
}

/// A box header at the reader's position: its type, header length and whole size, checked
/// against the `room` left in its parent.
fn box_header<R: Read>(r: &mut R, room: u64) -> Result<([u8; 4], u64, u64), String> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head).map_err(|_| "a box header is cut short".to_string())?;
    let kind: [u8; 4] = head[4..8].try_into().expect("four bytes");
    let (header, size) = match u32::from_be_bytes(head[0..4].try_into().expect("four bytes")) {
        0 => (8, room),
        1 => {
            let mut large = [0u8; 8];
            r.read_exact(&mut large).map_err(|_| "a box header is cut short".to_string())?;
            (16, u64::from_be_bytes(large))
        }
        n => (8, u64::from(n)),
    };
    if size < header || size > room {
        return Err(format!("its {} box claims {size} bytes where {room} are left", box_name(&kind)));
    }
    Ok((kind, header, size))
}

/// The child boxes of a box's content, in order.
fn children(mut data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>, String> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let mut cursor = data;
        let (kind, header, size) = box_header(&mut cursor, data.len() as u64)?;
        out.push((kind, &data[header as usize..size as usize]));
        data = &data[size as usize..];
    }
    Ok(out)
}

fn box_name(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

/// Big-endian reads that fail rather than panic on a short box.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        if self.0.len() < n {
            return Err(format!("its {what} is cut short"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self, what: &str) -> Result<u8, String> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &str) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.take(2, what)?.try_into().expect("two bytes")))
    }
    fn u32(&mut self, what: &str) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.take(4, what)?.try_into().expect("four bytes")))
    }
}

/// The primary item's turn from a `meta` box's content (a FullBox: version and flags first).
fn orientation_in_meta(meta: &[u8]) -> Result<u8, String> {
    let body = meta.get(4..).ok_or("its meta box is cut short")?;
    let boxes = children(body)?;
    let primary = {
        let (_, pitm) = boxes.iter().find(|(k, _)| k == b"pitm").ok_or("it names no primary item (pitm)")?;
        let mut b = Bytes(pitm);
        let version = b.u8("pitm")?;
        b.take(3, "pitm")?;
        if version == 0 { u32::from(b.u16("pitm")?) } else { b.u32("pitm")? }
    };
    let (_, iprp) = boxes.iter().find(|(k, _)| k == b"iprp").ok_or("it has no item properties (iprp)")?;
    let iprp = children(iprp)?;
    let (_, ipco) = iprp.iter().find(|(k, _)| k == b"ipco").ok_or("it has no property container (ipco)")?;
    let properties = children(ipco)?;
    let mut associated: Option<Vec<u16>> = None;
    for (_, ipma) in iprp.iter().filter(|(k, _)| k == b"ipma") {
        for (item, indices) in associations(ipma)? {
            if item == primary {
                associated.get_or_insert_with(Vec::new).extend(indices);
            }
        }
    }
    let associated = associated.ok_or_else(|| format!("its primary item {primary} has no properties"))?;

    let mut turn = 1u8;
    let mut turned = false;
    for index in associated {
        if index == 0 {
            continue; // "no property", per the format.
        }
        let (kind, payload) = properties
            .get(usize::from(index) - 1)
            .ok_or_else(|| format!("its primary item names property {index} of {}", properties.len()))?;
        let step = match kind {
            // `angle` (the low two bits) turns anticlockwise by angle x 90 degrees.
            b"irot" => [1, 8, 3, 6][usize::from(Bytes(payload).u8("irot")? & 0b11)],
            // The low bit, as libheif reads it (the preview's decoder): 0 exchanges top and
            // bottom, 1 exchanges left and right.
            b"imir" => [4, 2][usize::from(Bytes(payload).u8("imir")? & 1)],
            b"clap" if turned => return Err("its clean aperture (clap) follows a turn".into()),
            _ => continue,
        };
        turn = crate::xmp::compose_orientations(turn, step);
        turned = true;
    }
    Ok(turn)
}

/// One `ipma` box's associations: each item and its property indices (1-based), in order.
fn associations(ipma: &[u8]) -> Result<Vec<(u32, Vec<u16>)>, String> {
    let mut b = Bytes(ipma);
    let version = b.u8("ipma")?;
    let flags = b.take(3, "ipma")?[2];
    let count = b.u32("ipma")?;
    let mut out = Vec::new();
    for _ in 0..count {
        let item = if version < 1 { u32::from(b.u16("ipma")?) } else { b.u32("ipma")? };
        let n = b.u8("ipma")?;
        let mut indices = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            // The top bit marks the property essential; the rest is its index.
            indices.push(if flags & 1 == 1 { b.u16("ipma")? & 0x7fff } else { u16::from(b.u8("ipma")? & 0x7f) });
        }
        out.push((item, indices));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── hand-made boxes ────────────────────────────────────────────────────────

    fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut out = ((content.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(content);
        out
    }

    fn full(version: u8, flags: u32, content: &[u8]) -> Vec<u8> {
        let mut out = vec![version];
        out.extend_from_slice(&flags.to_be_bytes()[1..]);
        out.extend_from_slice(content);
        out
    }

    const FTYP: &[u8] = b"heic\0\0\0\0mif1heic";

    /// A HEIF holding `properties` (type, payload) and `ipma` (its version, flags and
    /// entries of item id and 1-based property indices), primary item `primary`.
    struct Heif {
        primary: u32,
        properties: Vec<(&'static [u8; 4], Vec<u8>)>,
        ipma_version: u8,
        ipma_flags: u32,
        entries: Vec<(u32, Vec<u16>)>,
    }

    impl Heif {
        fn primary_with(properties: Vec<(&'static [u8; 4], Vec<u8>)>) -> Self {
            let indices = (1..=properties.len() as u16).collect();
            Self { primary: 1, properties, ipma_version: 0, ipma_flags: 0, entries: vec![(1, indices)] }
        }

        fn ipma(&self) -> Vec<u8> {
            let mut content = (self.entries.len() as u32).to_be_bytes().to_vec();
            for (item, indices) in &self.entries {
                if self.ipma_version < 1 {
                    content.extend_from_slice(&(*item as u16).to_be_bytes());
                } else {
                    content.extend_from_slice(&item.to_be_bytes());
                }
                content.push(indices.len() as u8);
                for i in indices {
                    if self.ipma_flags & 1 == 1 {
                        content.extend_from_slice(&(0x8000 | i).to_be_bytes());
                    } else {
                        content.push(0x80 | *i as u8);
                    }
                }
            }
            bx(b"ipma", &full(self.ipma_version, self.ipma_flags, &content))
        }

        fn meta(&self) -> Vec<u8> {
            let pitm = if self.primary > 0xffff {
                full(1, 0, &self.primary.to_be_bytes())
            } else {
                full(0, 0, &(self.primary as u16).to_be_bytes())
            };
            let ipco: Vec<u8> = self.properties.iter().flat_map(|(k, p)| bx(k, p)).collect();
            let mut iprp = bx(b"ipco", &ipco);
            iprp.extend(self.ipma());
            let mut meta = bx(b"hdlr", &full(0, 0, b"\0\0\0\0pict\0\0\0\0\0\0\0\0\0\0\0\0\0"));
            meta.extend(bx(b"pitm", &pitm));
            meta.extend(bx(b"iprp", &iprp));
            bx(b"meta", &full(0, 0, &meta))
        }

        fn file(&self) -> Vec<u8> {
            let mut out = bx(b"ftyp", FTYP);
            out.extend(self.meta());
            out.extend(bx(b"mdat", &[0; 16]));
            out
        }
    }

    fn ispe() -> (&'static [u8; 4], Vec<u8>) {
        (b"ispe", full(0, 0, &[0, 0, 0x0f, 0xc0, 0, 0, 0x0b, 0xd0]))
    }

    /// Every `irot` angle, alone: anticlockwise quarter turns, as EXIF codes.
    #[test]
    fn irot_alone_is_its_anticlockwise_turn() {
        for (angle, exif) in [(0u8, 1u8), (1, 8), (2, 3), (3, 6)] {
            let file = Heif::primary_with(vec![ispe(), (b"irot", vec![angle])]).file();
            assert_eq!(orientation_of(&file), Ok(exif), "irot {angle}");
            // The six reserved bits are not the angle.
            let file = Heif::primary_with(vec![ispe(), (b"irot", vec![0b1111_1100 | angle])]).file();
            assert_eq!(orientation_of(&file), Ok(exif), "irot {angle} with reserved bits set");
        }
    }

    #[test]
    fn imir_alone_is_its_mirror() {
        assert_eq!(orientation_of(&Heif::primary_with(vec![(b"imir", vec![0])]).file()), Ok(4));
        assert_eq!(orientation_of(&Heif::primary_with(vec![(b"imir", vec![1])]).file()), Ok(2));
    }

    /// `irot` and `imir` apply in the order the item lists them, which matters: a quarter
    /// turn then a left-right mirror is a transpose (5), the mirror then the turn a
    /// transverse (7).
    #[test]
    fn irot_and_imir_compose_in_association_order() {
        let props = vec![(b"irot", vec![3]), (b"imir", vec![1])];
        let mut heif = Heif::primary_with(props);
        assert_eq!(orientation_of(&heif.file()), Ok(5), "turn, then mirror");
        heif.entries = vec![(1, vec![2, 1])];
        assert_eq!(orientation_of(&heif.file()), Ok(7), "mirror, then turn");
    }

    #[test]
    fn no_turn_is_orientation_one() {
        assert_eq!(orientation_of(&Heif::primary_with(vec![ispe()]).file()), Ok(1));
        // A property index of 0 means "none" and is skipped.
        let mut heif = Heif::primary_with(vec![ispe()]);
        heif.entries = vec![(1, vec![0, 1])];
        assert_eq!(orientation_of(&heif.file()), Ok(1));
    }

    /// Only the primary item's properties count: a thumbnail's (or a gain map's) turn is not
    /// the image's, and the primary's associations may be split over two `ipma` boxes.
    #[test]
    fn only_the_primary_items_associations_count() {
        let mut heif = Heif::primary_with(vec![ispe(), (b"irot", vec![1]), (b"irot", vec![3])]);
        heif.primary = 2;
        heif.entries = vec![(1, vec![1, 2]), (2, vec![1, 3])];
        assert_eq!(orientation_of(&heif.file()), Ok(6));
        heif.entries = vec![(2, vec![1])];
        let mut file = bx(b"ftyp", FTYP);
        let mut meta = heif.meta();
        // A second ipma for item 2 carrying its irot, appended inside iprp: rebuild by hand.
        let second = Heif { entries: vec![(2, vec![3])], ..Heif::primary_with(vec![]) }.ipma();
        let iprp_at = meta.windows(4).position(|w| w == b"iprp").unwrap() - 4;
        let iprp_len = u32::from_be_bytes(meta[iprp_at..iprp_at + 4].try_into().unwrap()) as usize;
        meta.splice(iprp_at + iprp_len..iprp_at + iprp_len, second.iter().copied());
        let grow = |buf: &mut Vec<u8>, at: usize| {
            let n = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) + second.len() as u32;
            buf[at..at + 4].copy_from_slice(&n.to_be_bytes());
        };
        grow(&mut meta, iprp_at);
        grow(&mut meta, 0);
        file.extend(meta);
        assert_eq!(orientation_of(&file), Ok(6), "split over two ipma boxes");
    }

    /// The wide forms: 32-bit item ids (`ipma` version 1, `pitm` version 1), 15-bit property
    /// indices (`ipma` flag 1), and a 64-bit box size.
    #[test]
    fn wide_ids_indices_and_sizes_are_read() {
        let mut props: Vec<(&'static [u8; 4], Vec<u8>)> = (0..200).map(|_| ispe()).collect();
        props.push((b"irot", vec![1]));
        let mut heif = Heif::primary_with(props);
        heif.primary = 0x0001_0002;
        heif.ipma_version = 1;
        heif.ipma_flags = 1;
        heif.entries = vec![(0x0001_0002, vec![1, 201])];
        assert_eq!(orientation_of(&heif.file()), Ok(8));

        let mut file = bx(b"ftyp", FTYP);
        let meta = Heif::primary_with(vec![(b"irot", vec![2])]).meta();
        file.extend(1u32.to_be_bytes());
        file.extend(b"meta");
        file.extend((meta.len() as u64 + 8).to_be_bytes());
        file.extend(&meta[8..]);
        assert_eq!(orientation_of(&file), Ok(3), "largesize meta");
    }

    /// The `meta` box need not come straight after `ftyp`, and a last box may run to the end
    /// of the file (size 0).
    #[test]
    fn meta_is_found_among_other_top_level_boxes() {
        let mut file = bx(b"ftyp", FTYP);
        file.extend(bx(b"free", &[0; 40]));
        file.extend(Heif::primary_with(vec![(b"irot", vec![3])]).meta());
        file.extend(0u32.to_be_bytes());
        file.extend(b"mdat");
        file.extend([0; 32]);
        assert_eq!(orientation_of(&file), Ok(6));
    }

    /// Whatever this reader cannot follow is an error, never "no turn".
    #[test]
    fn what_cannot_be_read_is_an_error() {
        let good = Heif::primary_with(vec![ispe(), (b"irot", vec![3])]);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("a JPEG", vec![0xff, 0xd8, 0xff, 0xe0, 0, 16, b'J', b'F', b'I', b'F']),
            ("empty", vec![]),
            ("no meta", bx(b"ftyp", FTYP)),
            ("cut short", good.file()[..60].to_vec()),
            ("a box larger than its parent", {
                let mut f = good.file();
                let at = f.windows(4).position(|w| w == b"ipco").unwrap() - 4;
                f[at..at + 4].copy_from_slice(&0xffffu32.to_be_bytes());
                f
            }),
            ("no pitm", {
                let mut f = bx(b"ftyp", FTYP);
                let mut meta = good.meta();
                let at = meta.windows(4).position(|w| w == b"pitm").unwrap();
                meta[at..at + 4].copy_from_slice(b"pitx");
                f.extend(meta);
                f
            }),
            ("the primary has no entry", Heif { primary: 9, ..Heif::primary_with(vec![(b"irot", vec![3])]) }.file()),
            ("an index past the properties", Heif {
                entries: vec![(1, vec![1, 7])],
                ..Heif::primary_with(vec![(b"irot", vec![3])])
            }.file()),
            ("an empty irot", Heif::primary_with(vec![(b"irot", vec![])]).file()),
            ("clap after irot", Heif::primary_with(vec![(b"irot", vec![3]), (b"clap", vec![0; 32])]).file()),
        ];
        for (case, file) in cases {
            assert!(orientation_of(&file).is_err(), "{case}: {:?}", orientation_of(&file));
        }
        // A clean aperture before the turn crops the stored frame it is measured in: fine.
        let clap_first = Heif::primary_with(vec![(b"clap", vec![0; 32]), (b"irot", vec![3])]);
        assert_eq!(orientation_of(&clap_first.file()), Ok(6));
    }

    // ── real files ─────────────────────────────────────────────────────────────

    /// `crates/core/tests/fixtures/heif/` (see its README): a 60x40 image, black with a white
    /// 5x5 block at x 10-14, y 5-9, encoded by `heif-enc` (libheif 1.23.4) with and without
    /// its turn options, some given an EXIF Orientation by exiftool 13.55. Each with the turn
    /// its container gives, and the EXIF Orientation exiftool reads from it.
    const FIXTURES: [(&str, u8, Option<u8>); 8] = [
        ("plain.heic", 1, None),
        ("rot90.heic", 6, None),
        ("fliph.heic", 2, None),
        ("flipv.heic", 4, None),
        ("rot90fliph.heic", 5, None),
        ("rot90_e6.heic", 6, Some(6)),
        ("rot90_e1.heic", 6, Some(1)),
        ("plain_e6.heic", 1, Some(6)),
    ];

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heif").join(name)
    }

    #[test]
    fn real_heif_files_give_their_container_turn() {
        for (name, turn, _) in FIXTURES {
            let path = fixture(name);
            assert!(is_heif(&path), "{name}");
            assert_eq!(container_orientation(&path), Ok(turn), "{name}");
        }
    }

    /// The premise of the HEIF frame (#154): ChairPhoto's HEIC preview — `magick` with
    /// `-auto-orient`, as `thumbnails` runs it — is the stored image turned by the container,
    /// exactly once, whatever the EXIF Orientation says. The white block is found in the
    /// preview and must sit where the image crate's own turn by [`container_orientation`]
    /// puts it, and nowhere the EXIF Orientation would put it instead. Skipped without a
    /// `magick` that decodes HEIC.
    #[test]
    fn container_turn_is_the_previews() {
        let decode = |path: &Path| -> Option<image::DynamicImage> {
            let out = std::process::Command::new("magick")
                .arg(path)
                .args(["-auto-orient", "png:-"])
                .output()
                .ok()?;
            out.status.success().then_some(())?;
            image::load_from_memory(&out.stdout).ok()
        };
        let Some(_) = decode(&fixture("plain.heic")) else {
            eprintln!("SKIPPED: container_turn_is_the_previews — no `magick` that decodes HEIC");
            return;
        };
        let mut stored = image::GrayImage::new(60, 40);
        for x in 10..15 {
            for y in 5..10 {
                stored.put_pixel(x, y, image::Luma([255]));
            }
        }
        let block = |img: &image::GrayImage| {
            let lit: Vec<(u32, u32)> =
                img.enumerate_pixels().filter(|(_, _, p)| p[0] > 128).map(|(x, y, _)| (x, y)).collect();
            assert!(!lit.is_empty(), "no white block");
            let n = lit.len() as f32;
            let c = lit.iter().fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + *x as f32, sy + *y as f32));
            (img.dimensions(), (c.0 / n, c.1 / n))
        };
        let turned = |o: u8| {
            let mut img = image::DynamicImage::ImageLuma8(stored.clone());
            img.apply_orientation(image::metadata::Orientation::from_exif(o).unwrap());
            block(&img.to_luma8())
        };
        for (name, turn, exif) in FIXTURES {
            let preview = block(&decode(&fixture(name)).expect(name).to_luma8());
            let want = turned(container_orientation(&fixture(name)).unwrap());
            assert_eq!(preview.0, want.0, "{name}: preview size");
            let off = ((preview.1 .0 - want.1 .0).abs(), (preview.1 .1 - want.1 .1).abs());
            assert!(off.0 <= 1.0 && off.1 <= 1.0, "{name}: block at {:?}, the container's turn {turn} puts it at {:?}", preview.1, want.1);
            if let Some(e) = exif.filter(|e| *e != 1) {
                let by_exif = turned(compose_after(turn, e));
                assert_ne!(preview, by_exif, "{name}: the preview applied EXIF {e} on top");
            }
        }
    }

    /// The turn `second` applied after `first`.
    fn compose_after(first: u8, second: u8) -> u8 {
        crate::xmp::compose_orientations(first, second)
    }
}
