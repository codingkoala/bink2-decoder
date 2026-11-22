//! Analysis tool: `bk2onestep <in.bk2> <frame> <ref.yuv> <out.yuv> [--no-loop-filter]`
//!
//! Decodes P frame `frame` against an externally supplied reference (raw planar 4:2:0, the
//! decoded previous frame, e.g. converted from RAD's own output) and writes the result as raw
//! 4:2:0. The decoder keeps no state between frames other than the reference picture.

use std::process::ExitCode;

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: bk2onestep <in.bk2> <frame> <ref.yuv> <out.yuv> [--no-loop-filter]");
        return ExitCode::from(2);
    }
    match run(&a) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(a: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(&a[1])?;
    let file = bink2::Container::parse(&data)?;
    let n: usize = a[2].parse()?;
    let refy = std::fs::read(&a[3])?;
    let mut dec = bink2::Decoder::new(&file.header)?;
    let opts = bink2::DecoderOptions {
        disable_loop_filter: a.get(5).map(|s| s.as_str()) == Some("--no-loop-filter"),
        ..Default::default()
    };
    dec.set_options(opts);
    let (w, h) = (file.header.width as usize, file.header.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if refy.len() != w * h + 2 * cw * ch {
        return Err("reference has the wrong size".into());
    }
    let (y, rest) = refy.split_at(w * h);
    let (u, v) = rest.split_at(cw * ch);
    dec.debug_set_reference_yuv(y, u, v);
    let pkt = file.frame(n)?;
    let f = dec.decode(pkt.video, pkt.keyframe)?;
    let mut out = f.y.clone();
    out.extend_from_slice(&f.u);
    out.extend_from_slice(&f.v);
    std::fs::write(&a[4], out)?;
    Ok(())
}
