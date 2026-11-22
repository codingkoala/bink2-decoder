//! Decodes one audio track of a `.bk2` file to a 16-bit PCM WAV file.
//!
//! ```text
//! cargo run --release --example bk2wav -- in.bk2 out.wav [track]
//! ```

use std::io::Write;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: bk2wav in.bk2 out.wav [track]");
        std::process::exit(2);
    }
    let track: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(0);
    let data = std::fs::read(&args[1])?;
    let file = bink2::Container::parse(&data)?;
    if file.header.audio_tracks.is_empty() {
        return Err("the file has no audio tracks".into());
    }
    let t0 = std::time::Instant::now();
    let (pcm, rate, channels) = bink2::decode_track(&file, track)?;
    eprintln!(
        "track {track}: {rate} Hz, {channels} ch, {} samples per channel ({:.2} s), decoded in {:.1} ms",
        pcm.len() / channels as usize,
        pcm.len() as f64 / channels as f64 / rate as f64,
        t0.elapsed().as_secs_f64() * 1000.0
    );

    let data_len = u32::try_from(pcm.len() * 2).map_err(|_| "too much audio for a WAV file")?;
    let block_align = channels * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::File::create(&args[2])?.write_all(&out)?;
    Ok(())
}
