# pHash Fixtures

`test-raw-video.mp4` is generated into this crate so `cargo test -p phash` does not depend on a fixture outside `crates/phash`.

Generation command:

```sh
ffmpeg -y -f lavfi -i testsrc2=duration=2:size=96x64:rate=5 -an -c:v mpeg4 -q:v 5 -pix_fmt yuv420p crates/phash/tests/fixtures/test-raw-video.mp4
```

Fixture properties:

- Video codec: `mpeg4`
- Size: `96x64`
- FPS: `5`
- Duration: `2.000000`
- Frames: `10`
- Video SHA-256: `efed6ca11b63766724aaf7121225a724a7a18842312e1bbe23e7ddc2b9c36a1f`
- Golden SHA-256: `ce79b2d5fc851af6408887ea7d4b6391ad2dc999e4e6aee0f5aed616ce27c89e`

`test_raw_video.offchain_binary_10x8_v1.txt` is the golden output for `test-raw-video.mp4` using the off-chain-compatible sequential frame decode and binary 10x8 pHash contract.

The source port was checked against off-chain commit `3a4072ae07b45875be29377cf3cecf52e80ca218`.

This fixture protects the migration from changing either the output format or frame-selection behavior.
