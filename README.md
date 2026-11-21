# bink2-decoder

An open decoder for Bink 2 (`.bk2`) video, written in Rust under the Apache License 2.0.

Status: specification draft. No decoder code yet.

- [docs/SPEC.md](docs/SPEC.md) describes the container and video bitstream.
- [docs/TABLES.md](docs/TABLES.md) holds the numeric constants.

## How this is built

The decoder is written from the documents in `docs/` only, as a clean-room implementation. The spec was written by studying an unmerged LGPL decoder (see the provenance note in SPEC.md). Implementation work must not read that code. Anything unclear in the spec gets fixed in the spec.

Bink and Bink Video are trademarks of their owner. This project is not affiliated with or endorsed by them, and it ships no third-party code or media.

## License

Apache License 2.0, see [LICENSE](LICENSE).
