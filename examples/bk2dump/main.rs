//! `bk2dump <in.bk2> <out.y4m | -> [max_frames] [options]`
//!
//! Prints the header and writes the decoded video as YUV4MPEG2 with the `C420jpeg` colour
//! space tag (planar 4:2:0, chroma planes ceil(w/2) x ceil(h/2)); `-` skips the Y4M output.
//! The alpha plane, if any, is not written. Bink 2 does not signal chroma siting or colour
//! range (SPEC open question 8), so the tag is a convention, not a statement about the source.
//!
//! Options:
//! - `--png DIR`: write frames as PNG into DIR (`frame_NNNN.png`), converted with BT.601
//!   limited range.
//! - `--png-frames LIST`: comma-separated frame numbers to export (default: all decoded).
//! - `--crop X,Y,W,H`: export only this rectangle (full resolution).
//! - `--half`: export at half size (2x2 average) after cropping.
//! - `--swap-uv`: swap the chroma planes in the PNG (to check the plane order).
//! - `--keep-going`: report decode errors and continue with the next frame.
//! - `--conceal`: conceal failed slices (copy from the previous frame) instead of failing the
//!   frame; the failures are still printed.
//! - `--report`: print per-slice bit accounting (where each slice ended vs. the slice table).

mod png;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;

#[derive(Default)]
struct Opts {
    input: String,
    output: String,
    max_frames: Option<usize>,
    png_dir: Option<String>,
    png_frames: Option<Vec<usize>>,
    crop: Option<[usize; 4]>,
    half: bool,
    swap_uv: bool,
    keep_going: bool,
    conceal: bool,
    report: bool,
}

const USAGE: &str = "usage: bk2dump <in.bk2> <out.y4m | -> [max_frames] [--png DIR] [--png-frames N,N,..] \
                     [--crop X,Y,W,H] [--half] [--swap-uv] [--keep-going] [--conceal] [--report]";

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut pos = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--png" => o.png_dir = Some(val()?),
            "--png-frames" => {
                let v = val()?;
                let list = v
                    .split(',')
                    .map(|s| s.trim().parse::<usize>())
                    .collect::<Result<Vec<_>, _>>();
                o.png_frames = Some(list.map_err(|_| "bad --png-frames list".to_string())?);
            }
            "--crop" => {
                let v = val()?;
                let n: Vec<usize> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                if n.len() != 4 {
                    return Err("--crop needs X,Y,W,H".into());
                }
                o.crop = Some([n[0], n[1], n[2], n[3]]);
            }
            "--half" => o.half = true,
            "--swap-uv" => o.swap_uv = true,
            "--keep-going" => o.keep_going = true,
            "--conceal" => o.conceal = true,
            "--report" => o.report = true,
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => pos.push(a),
        }
    }
    if pos.len() < 2 || pos.len() > 3 {
        return Err(USAGE.into());
    }
    o.input = pos[0].clone();
    o.output = pos[1].clone();
    if let Some(n) = pos.get(2) {
        o.max_frames = Some(n.parse().map_err(|_| "max_frames must be a number".to_string())?);
    }
    Ok(o)
}

fn main() -> ExitCode {
    let o = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match run(&o) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// BT.601 limited-range YCbCr to RGB.
fn to_rgb(f: &bink2::VideoFrame, o: &Opts) -> (usize, usize, Vec<u8>) {
    let [cx, cy, cw, ch] = o.crop.unwrap_or([0, 0, f.width, f.height]);
    let cx = cx.min(f.width);
    let cy = cy.min(f.height);
    let cw = cw.min(f.width - cx);
    let ch = ch.min(f.height - cy);
    let (u, v) = if o.swap_uv { (&f.v, &f.u) } else { (&f.u, &f.v) };
    let cwid = f.chroma_width();
    let px = |x: usize, y: usize| -> [f32; 3] {
        let yy = f.y[y * f.width + x] as f32 - 16.0;
        let cb = u[(y / 2) * cwid + x / 2] as f32 - 128.0;
        let cr = v[(y / 2) * cwid + x / 2] as f32 - 128.0;
        [
            1.164 * yy + 1.596 * cr,
            1.164 * yy - 0.392 * cb - 0.813 * cr,
            1.164 * yy + 2.017 * cb,
        ]
    };
    let s = if o.half { 2 } else { 1 };
    let (ow, oh) = (cw / s, ch / s);
    let mut rgb = Vec::with_capacity(ow * oh * 3);
    for y in 0..oh {
        for x in 0..ow {
            let mut acc = [0f32; 3];
            for dy in 0..s {
                for dx in 0..s {
                    let p = px(cx + x * s + dx, cy + y * s + dy);
                    for c in 0..3 {
                        acc[c] += p[c];
                    }
                }
            }
            for a in acc {
                rgb.push((a / (s * s) as f32).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    (ow, oh, rgb)
}

fn run(o: &Opts) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(&o.input)?;
    let file = bink2::Container::parse(&data)?;
    let h = &file.header;
    println!("revision:     {}", h.revision as char);
    println!(
        "variant:      {}",
        if h.is_float_variant() { "float" } else { "integer" }
    );
    println!("size:         {}x{}", h.width, h.height);
    println!(
        "fps:          {}/{} ({:.3})",
        h.fps_num,
        h.fps_den,
        h.fps_num as f64 / h.fps_den as f64
    );
    println!("frames:       {}", h.num_frames);
    println!("alpha:        {}", if h.has_alpha() { "yes" } else { "no" });
    println!("video flags:  0x{:08x}", h.video_flags);
    if file.base_offset != 0 {
        println!("SMUS wrapped: data at offset {}", file.base_offset);
    }
    println!("audio tracks: {}", h.audio_tracks.len());
    for (i, t) in h.audio_tracks.iter().enumerate() {
        println!(
            "  #{i}: id {} {} Hz {} {}{}",
            t.id,
            t.sample_rate,
            if t.is_stereo() { "stereo" } else { "mono" },
            if t.is_dct() { "DCT" } else { "RDFT" },
            if t.is_16bit() { " 16-bit" } else { "" }
        );
    }

    let mut y4m = if o.output == "-" {
        None
    } else {
        let mut out = BufWriter::new(File::create(&o.output)?);
        writeln!(
            out,
            "YUV4MPEG2 W{} H{} F{}:{} Ip A1:1 C420jpeg",
            h.width, h.height, h.fps_num, h.fps_den
        )?;
        Some(out)
    };
    if let Some(d) = &o.png_dir {
        std::fs::create_dir_all(d)?;
    }
    let mut dec = bink2::Decoder::new(h)?;
    dec.set_options(bink2::DecoderOptions {
        conceal_slice_errors: o.conceal,
        ..Default::default()
    });
    let (mut slices_total, mut slices_sync) = (0usize, 0usize);
    let n = file.num_frames().min(o.max_frames.unwrap_or(usize::MAX));
    let (mut ok, mut failed) = (0usize, 0usize);
    for i in 0..n {
        let res = file.frame(i).and_then(|pkt| dec.decode(pkt.video, pkt.keyframe));
        let reps = dec.slice_reports();
        slices_total += reps.len();
        slices_sync += reps.iter().filter(|r| r.in_sync()).count();
        if o.report || o.conceal {
            for r in reps {
                if o.report || r.failure.is_some() {
                    let end = r.consumed_bit.div_ceil(8);
                    let fail = match &r.failure {
                        Some(f) => format!(" FAILED at MB ({},{}) bit {}: {}", f.mb.0, f.mb.1, f.bit, f.error),
                        None => String::new(),
                    };
                    println!(
                        "frame {i} flags 0x{:08x} slice {} rows {}..{} bits {}..{} -> byte {end} of {}{}{fail}",
                        dec.last_frame_flags(),
                        r.index,
                        r.mb_rows.0,
                        r.mb_rows.1,
                        r.start_bit,
                        r.consumed_bit,
                        r.end_byte,
                        if r.in_sync() { " in sync" } else { "" }
                    );
                }
            }
        }
        let f = match res {
            Ok(f) => f,
            Err(e) if o.keep_going => {
                eprintln!("frame {i}: {e}");
                failed += 1;
                continue;
            }
            Err(e) => return Err(format!("frame {i}: {e}").into()),
        };
        ok += 1;
        if let Some(out) = y4m.as_mut() {
            out.write_all(b"FRAME\n")?;
            out.write_all(&f.y)?;
            out.write_all(&f.u)?;
            out.write_all(&f.v)?;
        }
        if let Some(d) = &o.png_dir {
            if o.png_frames.as_ref().is_none_or(|l| l.contains(&i)) {
                let (w, hh, rgb) = to_rgb(&f, o);
                let path = format!("{d}/frame_{i:04}.png");
                png::write_png(BufWriter::new(File::create(&path)?), w, hh, &rgb)?;
                println!("wrote {path} ({}keyframe)", if f.keyframe { "" } else { "not a " });
            }
        }
    }
    if let Some(mut out) = y4m {
        out.flush()?;
    }
    println!("decoded {ok} of {n} frames, {failed} failed; {slices_sync} of {slices_total} slices in sync");
    Ok(())
}
