//! `bk2dump <in.bk2> <out.y4m> [max_frames]`
//!
//! Prints the header and writes the decoded video as YUV4MPEG2 with the `C420jpeg` colour
//! space tag (planar 4:2:0, chroma planes ceil(w/2) x ceil(h/2)). The alpha plane, if any, is
//! not written. Bink 2 does not signal chroma siting or colour range (SPEC open question 8),
//! so the tag is a convention, not a statement about the source.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("usage: bk2dump <in.bk2> <out.y4m> [max_frames]");
        return ExitCode::from(2);
    }
    let max_frames = match args.get(3).map(|s| s.parse::<usize>()) {
        None => usize::MAX,
        Some(Ok(n)) => n,
        Some(Err(_)) => {
            eprintln!("max_frames must be a number");
            return ExitCode::from(2);
        }
    };
    match run(&args[1], &args[2], max_frames) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(input: &str, output: &str, max_frames: usize) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(input)?;
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

    let mut out = BufWriter::new(File::create(output)?);
    writeln!(
        out,
        "YUV4MPEG2 W{} H{} F{}:{} Ip A1:1 C420jpeg",
        h.width, h.height, h.fps_num, h.fps_den
    )?;
    let mut dec = bink2::Decoder::new(h)?;
    let n = file.num_frames().min(max_frames);
    for i in 0..n {
        let pkt = file.frame(i).map_err(|e| format!("frame {i}: {e}"))?;
        let f = dec
            .decode(pkt.video, pkt.keyframe)
            .map_err(|e| format!("frame {i}: {e}"))?;
        out.write_all(b"FRAME\n")?;
        out.write_all(&f.y)?;
        out.write_all(&f.u)?;
        out.write_all(&f.v)?;
    }
    out.flush()?;
    println!("wrote {n} frames to {output}");
    Ok(())
}
