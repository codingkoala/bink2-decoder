# bink2-decoder

An open decoder for Bink 2 (`.bk2`) video and Bink Audio, written in Rust under the Apache License 2.0.

Status: first implementation of the container and both video variants (float `a`/`d`/`f`, integer `g`+). It is tested on synthetic streams only and has not been checked against real files yet.

```
cargo test
cargo run --release --example bk2dump -- in.bk2 out.y4m [max_frames]
cargo run --release --example bk2wav -- in.bk2 out.wav [track]
```

`bk2dump` prints the header and writes YUV4MPEG2 (`C420jpeg`, alpha dropped).

Integer-variant frames go through the in-loop filter of SPEC §4.10 (`DecoderOptions::disable_loop_filter` or `bk2dump --no-loop-filter` switch it off for debugging). Unverified spec behaviours are marked `SPEC-QUESTION` in the code. `Decoder::set_options` selects how out-of-picture motion vectors are handled.

Audio: `AudioDecoder` (and `decode_track` for a whole track) decodes Bink Audio with the DCT codec, mono or stereo, versions 1 and 2, to interleaved 16-bit PCM. Version 2 at 48 kHz stereo was checked against RAD's own output: same sample count, 99.92 % of samples identical, none off by more than 1. The RDFT codec is not supported. `bk2wav` writes a track to a WAV file.

- [docs/AUDIO.md](docs/AUDIO.md) describes Bink Audio.
- [docs/SPEC.md](docs/SPEC.md) describes the container and video bitstream.
- [docs/TABLES.md](docs/TABLES.md) holds the numeric constants.

## How this is built

The decoder is written from the documents in `docs/` only, as a clean-room implementation. The spec was written by studying an unmerged LGPL decoder (see the provenance note in SPEC.md). Implementation work must not read that code. Anything unclear in the spec gets fixed in the spec.

Bink and Bink Video are trademarks of their owner. This project is not affiliated with or endorsed by them, and it ships no third-party code or media.

## License

Apache License 2.0, see [LICENSE](LICENSE).
