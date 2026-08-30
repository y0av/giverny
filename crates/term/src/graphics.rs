//! The kitty graphics protocol: images in the grid.
//!
//! Programs send images as APC sequences — `ESC _ G <key>=<value>,… ; <base64>
//! ESC \` — which `vte` has no callback for and `alacritty_terminal` does not
//! implement, so the byte tee scans for them itself. Payloads arrive in chunks
//! of at most 4096 bytes with `m=1` on every chunk but the last.
//!
//! Scope, deliberately: transmit (direct or from a file), display, delete, and
//! the support query that programs use to decide whether to bother. PNG and
//! raw RGB/RGBA. Not implemented: animation, shared memory, z-index ordering,
//! Unicode placeholders.

use std::collections::HashMap;

/// A decoded image the terminal is holding on to.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major.
    pub rgba: Vec<u8>,
}

/// An image placed on the grid.
#[derive(Debug, Clone)]
pub struct Placement {
    pub image_id: u32,
    /// Column of the top-left cell.
    pub col: u16,
    /// Row at placement time, counted from the start of scrollback, so the
    /// image scrolls with the text that produced it.
    pub abs_row: i64,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Default)]
struct Pending {
    id: u32,
    format: u32,
    width: u32,
    height: u32,
    /// `a=T` — display as soon as the payload is complete.
    display: bool,
    cols: u16,
    rows: u16,
    data: Vec<u8>,
}

/// Everything the terminal knows about images.
#[derive(Debug, Default)]
pub struct Graphics {
    pub images: HashMap<u32, Image>,
    pub placements: Vec<Placement>,
    pending: Option<Pending>,
    /// Bumped whenever the picture changes, so the renderer knows to re-upload.
    pub generation: u64,
}

/// What the terminal must write back, and whether the screen changed.
#[derive(Debug, Default)]
pub struct Response {
    pub reply: Option<Vec<u8>>,
    pub dirty: bool,
}

fn parse_keys(head: &str) -> HashMap<&str, &str> {
    head.split(',')
        .filter_map(|pair| pair.split_once('='))
        .collect()
}

fn num<T: std::str::FromStr>(keys: &HashMap<&str, &str>, key: &str) -> Option<T> {
    keys.get(key)?.parse().ok()
}

impl Graphics {
    /// Handle one APC payload (everything between `ESC _` and `ESC \`).
    ///
    /// `cursor` is where the cursor is now, in (absolute row, column), because
    /// a placement lands at the cursor.
    pub fn apc(&mut self, payload: &[u8], cursor: (i64, u16)) -> Response {
        // `G` marks it as ours; anything else is some other APC user's.
        let Some(body) = payload.strip_prefix(b"G") else {
            return Response::default();
        };
        let split = body.iter().position(|b| *b == b';');
        let (head, data) = match split {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &body[..0]),
        };
        let Ok(head) = std::str::from_utf8(head) else {
            return Response::default();
        };
        let keys = parse_keys(head);
        let action = keys.get("a").copied().unwrap_or("t");
        let id: u32 = num(&keys, "i").unwrap_or(0);
        let quiet: u32 = num(&keys, "q").unwrap_or(0);

        match action {
            // Support probe. Programs send a 1x1 image and look for `OK`; a
            // terminal that stays silent is treated as having no graphics.
            "q" => Response {
                reply: (quiet == 0).then(|| format!("\x1b_Gi={id};OK\x1b\\").into_bytes()),
                dirty: false,
            },
            "d" => {
                self.delete(id);
                Response {
                    reply: None,
                    dirty: true,
                }
            }
            "t" | "T" => self.transmit(&keys, data, action == "T", cursor, quiet),
            "p" => {
                let placed = self.place(id, &keys, cursor);
                Response {
                    reply: (quiet == 0 && placed)
                        .then(|| format!("\x1b_Gi={id};OK\x1b\\").into_bytes()),
                    dirty: placed,
                }
            }
            _ => Response::default(),
        }
    }

    fn transmit(
        &mut self,
        keys: &HashMap<&str, &str>,
        data: &[u8],
        display: bool,
        cursor: (i64, u16),
        quiet: u32,
    ) -> Response {
        let more = num::<u32>(keys, "m").unwrap_or(0) == 1;
        let first = self.pending.is_none();
        let pending = self.pending.get_or_insert_with(Pending::default);
        if first {
            pending.id = num(keys, "i").unwrap_or(0);
            pending.format = num(keys, "f").unwrap_or(32);
            pending.width = num(keys, "s").unwrap_or(0);
            pending.height = num(keys, "v").unwrap_or(0);
            pending.display = display;
            pending.cols = num(keys, "c").unwrap_or(0);
            pending.rows = num(keys, "r").unwrap_or(0);
        }
        // Continuation chunks carry only `m` and payload.
        pending.data.extend_from_slice(data);
        if more {
            return Response::default();
        }

        let Some(p) = self.pending.take() else {
            return Response::default();
        };
        let transmission = keys.get("t").copied().unwrap_or("d");
        let bytes = match decode_payload(&p.data, transmission) {
            Some(b) => b,
            None => return Response::default(),
        };
        let Some(image) = decode_image(&bytes, p.format, p.width, p.height) else {
            return Response {
                reply: (quiet < 2).then(|| format!("\x1b_Gi={};EBADPNG\x1b\\", p.id).into_bytes()),
                dirty: false,
            };
        };
        let id = p.id;
        self.images.insert(id, image);
        let mut dirty = false;
        if p.display {
            let mut keys = keys.clone();
            let (c, r) = (p.cols.to_string(), p.rows.to_string());
            keys.insert("c", &c);
            keys.insert("r", &r);
            dirty = self.place(id, &keys, cursor);
        }
        self.generation += 1;
        Response {
            reply: (quiet == 0).then(|| format!("\x1b_Gi={id};OK\x1b\\").into_bytes()),
            dirty,
        }
    }

    fn place(&mut self, id: u32, keys: &HashMap<&str, &str>, cursor: (i64, u16)) -> bool {
        let Some(image) = self.images.get(&id) else {
            return false;
        };
        // Cell size is the program's business when it says so; otherwise the
        // caller scales by natural size later.
        let cols: u16 = num(keys, "c").unwrap_or(0);
        let rows: u16 = num(keys, "r").unwrap_or(0);
        let (w, h) = (image.width.max(1), image.height.max(1));
        self.placements.push(Placement {
            image_id: id,
            col: cursor.1,
            abs_row: cursor.0,
            cols,
            rows,
        });
        let _ = (w, h);
        self.generation += 1;
        true
    }

    /// `a=d`: id 0 clears everything, which is what programs send on exit.
    pub fn delete(&mut self, id: u32) {
        if id == 0 {
            self.placements.clear();
            self.images.clear();
        } else {
            self.placements.retain(|p| p.image_id != id);
            self.images.remove(&id);
        }
        self.generation += 1;
    }

    /// Forget placements that have scrolled out of the retained scrollback,
    /// and any image nothing points at any more.
    pub fn prune(&mut self, oldest_row: i64) {
        let before = self.placements.len();
        self.placements.retain(|p| p.abs_row >= oldest_row);
        if self.placements.len() != before {
            let live: Vec<u32> = self.placements.iter().map(|p| p.image_id).collect();
            self.images.retain(|id, _| live.contains(id));
            self.generation += 1;
        }
    }
}

fn decode_payload(data: &[u8], transmission: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    match transmission {
        // Direct: the payload is the image.
        "d" => Some(decoded),
        // File or temp file: the payload is a path. Only local paths — there
        // is no reason for a terminal to fetch anything.
        "f" | "t" => {
            let path = String::from_utf8(decoded).ok()?;
            std::fs::read(path).ok()
        }
        _ => None,
    }
}

fn decode_image(bytes: &[u8], format: u32, width: u32, height: u32) -> Option<Image> {
    match format {
        // PNG: dimensions come from the file.
        100 => {
            let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
            let mut reader = decoder.read_info().ok()?;
            let mut buf = vec![0; reader.output_buffer_size()?];
            let info = reader.next_frame(&mut buf).ok()?;
            let rgba = to_rgba(&buf[..info.buffer_size()], info.color_type, info.bit_depth)?;
            Some(Image {
                width: info.width,
                height: info.height,
                rgba,
            })
        }
        // Raw pixels, dimensions from the escape.
        24 | 32 => {
            let (w, h) = (width, height);
            if w == 0 || h == 0 {
                return None;
            }
            let px = (w as usize).checked_mul(h as usize)?;
            let rgba = if format == 32 {
                if bytes.len() < px * 4 {
                    return None;
                }
                bytes[..px * 4].to_vec()
            } else {
                if bytes.len() < px * 3 {
                    return None;
                }
                let mut out = Vec::with_capacity(px * 4);
                for chunk in bytes[..px * 3].as_chunks::<3>().0 {
                    out.extend_from_slice(chunk);
                    out.push(255);
                }
                out
            };
            Some(Image {
                width: w,
                height: h,
                rgba,
            })
        }
        _ => None,
    }
}

fn to_rgba(buf: &[u8], color: png::ColorType, depth: png::BitDepth) -> Option<Vec<u8>> {
    if depth != png::BitDepth::Eight {
        return None;
    }
    Some(match color {
        png::ColorType::Rgba => buf.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(buf.len() / 3 * 4);
            for c in buf.as_chunks::<3>().0 {
                out.extend_from_slice(c);
                out.push(255);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(buf.len() * 4);
            for g in buf {
                out.extend_from_slice(&[*g, *g, *g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(buf.len() * 2);
            for c in buf.as_chunks::<2>().0 {
                out.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
            out
        }
        png::ColorType::Indexed => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer
                .write_image_data(&vec![0x7fu8; (w * h * 4) as usize])
                .unwrap();
        }
        out
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn a_support_query_is_answered_or_programs_assume_no_graphics() {
        let mut g = Graphics::default();
        // What a program actually sends to probe: a 1x1 RGB image.
        let r = g.apc(b"Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA", (0, 0));
        assert_eq!(r.reply.unwrap(), b"\x1b_Gi=31;OK\x1b\\");
        // `q=1` asks for silence.
        let r = g.apc(b"Gi=31,s=1,v=1,a=q,q=1,t=d,f=24;AAAA", (0, 0));
        assert!(r.reply.is_none());
    }

    #[test]
    fn a_png_transmitted_and_displayed_lands_at_the_cursor() {
        let mut g = Graphics::default();
        let payload = b64(&png_bytes(4, 2));
        let seq = format!("Gi=7,a=T,f=100,c=3,r=2;{payload}");
        let r = g.apc(seq.as_bytes(), (12, 5));
        assert!(r.dirty, "displaying changes the screen");
        let image = g.images.get(&7).expect("image stored");
        assert_eq!((image.width, image.height), (4, 2));
        assert_eq!(image.rgba.len(), 4 * 2 * 4);
        let p = &g.placements[0];
        assert_eq!((p.abs_row, p.col), (12, 5), "placed at the cursor");
        assert_eq!((p.cols, p.rows), (3, 2));
    }

    #[test]
    fn chunked_payloads_are_reassembled() {
        // Real transmissions split base64 into <=4096-byte chunks with m=1 on
        // every chunk but the last; only the first carries the metadata.
        let mut g = Graphics::default();
        let payload = b64(&png_bytes(2, 2));
        let (a, b) = payload.split_at(payload.len() / 2);
        let r = g.apc(format!("Gi=9,a=T,f=100,m=1;{a}").as_bytes(), (0, 0));
        assert!(r.reply.is_none(), "no answer until the last chunk");
        assert!(g.images.is_empty());
        let r = g.apc(format!("Gm=0;{b}").as_bytes(), (0, 0));
        assert_eq!(r.reply.unwrap(), b"\x1b_Gi=9;OK\x1b\\");
        assert!(g.images.contains_key(&9), "reassembled");
    }

    #[test]
    fn a_file_transmission_reads_the_path() {
        let mut g = Graphics::default();
        let path = std::env::temp_dir().join(format!("giverny-gfx-{}.png", std::process::id()));
        std::fs::write(&path, png_bytes(3, 3)).unwrap();
        let seq = format!(
            "Gi=4,a=T,f=100,t=f;{}",
            b64(path.display().to_string().as_bytes())
        );
        g.apc(seq.as_bytes(), (0, 0));
        assert!(g.images.contains_key(&4), "image read from the file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_clears_what_it_says() {
        let mut g = Graphics::default();
        g.apc(
            format!("Gi=1,a=T,f=100;{}", b64(&png_bytes(2, 2))).as_bytes(),
            (0, 0),
        );
        g.apc(
            format!("Gi=2,a=T,f=100;{}", b64(&png_bytes(2, 2))).as_bytes(),
            (0, 0),
        );
        g.apc(b"Ga=d,i=1", (0, 0));
        assert!(!g.images.contains_key(&1));
        assert!(g.images.contains_key(&2), "only the named image went");
        // id 0 is "everything", which programs send when they exit.
        g.apc(b"Ga=d,i=0", (0, 0));
        assert!(g.images.is_empty() && g.placements.is_empty());
    }

    #[test]
    fn images_that_scroll_out_of_history_are_forgotten() {
        let mut g = Graphics::default();
        g.apc(
            format!("Gi=1,a=T,f=100;{}", b64(&png_bytes(2, 2))).as_bytes(),
            (5, 0),
        );
        g.apc(
            format!("Gi=2,a=T,f=100;{}", b64(&png_bytes(2, 2))).as_bytes(),
            (500, 0),
        );
        g.prune(100);
        assert_eq!(g.placements.len(), 1, "the old placement went");
        assert!(!g.images.contains_key(&1), "and its pixels with it");
        assert!(g.images.contains_key(&2));
    }

    #[test]
    fn a_corrupt_image_is_reported_not_swallowed() {
        let mut g = Graphics::default();
        let r = g.apc(
            format!("Gi=3,a=T,f=100;{}", b64(b"not a png")).as_bytes(),
            (0, 0),
        );
        assert_eq!(r.reply.unwrap(), b"\x1b_Gi=3;EBADPNG\x1b\\");
        assert!(g.images.is_empty());
    }

    #[test]
    fn other_apc_users_are_left_alone() {
        let mut g = Graphics::default();
        // Not a graphics command: no `G` prefix.
        let r = g.apc(b"Xsomething-else", (0, 0));
        assert!(r.reply.is_none() && !r.dirty);
    }
}
