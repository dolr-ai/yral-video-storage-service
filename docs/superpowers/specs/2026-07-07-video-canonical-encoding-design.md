# Video Canonical Encoding & Faststart — Design Spec

**Status:** Draft (rev 2) for team review — please review, verify, and confirm before implementation starts.
**Tickets:** [yral#1992](https://github.com/dolr-ai/yral/issues/1992), [yral#2077](https://github.com/dolr-ai/yral/issues/2077) (duplicate of #1992 — suggest closing one against the other)
**Repo:** yral-video-storage-service
**Date:** 2026-07-08 (rev 2 — reweighted for videogen-only ingress; rev 1 was 2026-07-07)
**Author:** Prakash

---

## 0. Recommendation (TL;DR)

New content is AI-generated (LTX-2, Wan 2.5) and arrives pre-standardized by the providers — the mobile app's user upload path has been removed. So:

1. **Ingest gate (small):** add a cheap probe → fix-moov-if-needed gate at the existing upload choke point. Full re-encode exists only as a rarely-fired fallback branch (and future-proofing if user uploads return). ~1–2 days.
2. **Catalog backfill (the actual project):** one-time lossless repackage of the existing ~600k-video catalog to move the moov atom to the front — that catalog is what the feed serves and where the slow videos live. ~1 week elapsed, reusing the pHash-backfill fleet infra.
3. **Heavy tail (data-driven follow-up):** old oversized user uploads are now a *frozen, finite* set. The backfill's probe pass yields a bitrate distribution for free; then a one-time decision to re-encode the worst offenders or leave them.
4. **Not now:** HLS/DASH, HEVC, AV1-as-standard. Re-evaluate HLS only if stall metrics stay bad after 1–3 ship (a keyframe-cadence choice below keeps that path cheap).

Everything below is the evidence and detail behind those four lines.

## 1. Problem

On poor connections, feed videos take too long to show the first frame. Two independent causes, both server-side:

1. **Moov atom at the end of the file.** MP4s were historically stored as uploaded. Cameras and most encoders write the `moov` atom (the index the player needs before it can render anything) *after* the media data. The player must either download the whole file or issue extra ranged requests to find it — on a slow link that is seconds-to-minutes of blank screen before the first frame.
2. **No encoding standardization.** Files were preserved at original resolution/bitrate/codec. A legacy 4K60 HEVC upload is stored and served as 4K60 HEVC: oversized for a phone feed, possibly software-decoded on older devices, and far above the bandwidth of a weak connection.

**Key context (changed since the tickets were filed):** the mobile app no longer offers user video upload — all new content arrives via the videogen (AI generation) path. AI providers emit standardized H.264 MP4 at feed-appropriate sizes, so the un-standardized-encoding problem is concentrated almost entirely in the **existing catalog**, and is no longer growing. The moov-position problem applies to both old catalog and (potentially) new videogen output — providers do not guarantee faststart.

The tickets ask for a researched recommendation on standardized re-encoding, plus moov-to-front for the MP4s we serve to the feed. The latter explicitly covers **existing** catalog videos — backfill is the first-class deliverable.

## 2. Verified current state

Code claims verified against `main` (3303d9b) on 2026-07-07/08.

| Fact | Evidence |
|---|---|
| Videogen is the only live ingress; its upload URL is minted by `reserve_upload_destination` and points at `POST /duplicate_raw/upload` on this service — one choke point, bytes flow through us | `src/routes/videogen/generate.rs:1100-1129`, `src/routes/upload/get_upload_url.rs:37-53` |
| Live providers: LTX-2, Wan 2.5 | `src/routes/videogen/generate.rs:713,1627` |
| The server holds the **full video bytes in memory** during the upload request (multipart, 500 MB cap) — inline processing is feasible | `src/routes/duplicate.rs:522-538` |
| Upload is two-phase: `initial` stages to Storj/S3 with a TTL; `finalize` downloads the staged file to `/tmp` via `uplink`, then re-uploads with final metadata | `src/routes/duplicate.rs:624-820` |
| SFW uploads are written to **both** Storj and S3 (four parallel uploads incl. thumbnails); NSFW to Storj only | `src/routes/duplicate.rs:549-603` |
| ffmpeg is already installed in the prod image and already spawned for thumbnail extraction | `deploy/Dockerfile:16`, `src/thumbnail.rs:40` |
| Master table already has the columns this feature needs: `moov_atom_front BOOLEAN`, `canonical_encoding_version TEXT`, plus width/height/fps/codec/container fields, upserted with COALESCE (later fills don't clobber earlier values) | `src/media_index/schema.rs:26-27`, `src/media_index/repo.rs:156-157` |
| Precedent for best-effort inline master registration from a request handler exists (videogen completion hook) | `src/jobs/ingest.rs:107-114` |
| A sharded, leased-job backfill fleet pattern exists and has run at catalog scale (pHash backfill, 3 boxes) | `docs/superpowers/plans/2026-06-24-sharded-phash-backfill.md` |

### Ingress inventory

| Path | What it is | Status / coverage |
|---|---|---|
| Videogen (`/api/v2/videogen/*` → `/duplicate_raw/upload` + `finalize`) | AI-generated videos, the only live ingress | **Covered — Phase 1a gate** |
| Direct mobile/web user upload (`/get-upload-url` → `/duplicate_raw/upload`) | **Removed from the app.** Routes still exist server-side | Gate covers them automatically if the path ever returns |
| `/duplicate`, `/hls/duplicate` | Legacy Cloudflare Stream pulls (`src/routes/duplicate.rs:394`) | Presumed dead with app upload removed — **confirm and consider deleting (Q6)** |
| Existing catalog (~600k+ objects in master) | Already in Storj/S3, served to feed today | **Covered — Phase 1b backfill (primary workstream)** |

## 3. Options considered

### Server-side

- **A. Faststart-only remux** (`-c copy -movflags +faststart`, no re-encode): seconds per video, zero quality loss, pHash-stable. Fixes moov but not oversized bitrates.
- **B. Canonical single-rendition re-encode** (H.264 progressive MP4, capped resolution/fps, capped CRF, faststart): fixes moov *and* size/compat.
- **C. Adaptive bitrate (HLS/DASH)**: best possible poor-network behavior, but multiple renditions, playlist management, ~3× storage, and a mobile player rewrite. Weeks-to-months.

**Decision: A as the workhorse (ingest gate + entire catalog backfill), B as the ingest fallback branch and a possible targeted pass over the heavy tail, C deferred.** Rationale: new content is already provider-standardized, so blanket re-encoding buys almost nothing and risks quality; remux is lossless and cheap enough to run over 600k videos; and for sub-60s feed clips a well-formed progressive MP4 plus client preloading is the proven baseline. HLS is re-evaluated in Phase 3 only if metrics still show buffering after Phases 1–2 land.

Middle options deliberately kept in the back pocket, gated on metrics, in order: a second 480p rendition for bad networks (poor-man's ABR, no player rewrite) → an AV1 rendition for capable devices → full HLS.

### Client-side (context, different repo/team)

Preloading + player recycling (3-instance pattern, ranged prefetch of the first ~1 MB of upcoming videos) is what makes faststart pay off as *instant* TTFF. Out of scope for this service; tracked as Phase 2 for the mobile team.

## 4. Canonical format v1

Applies to the ingest gate's transcode branch and to any heavy-tail re-encode. (Remux outcomes keep source encoding and only fix the container layout.)

| Parameter | Value | Rationale |
|---|---|---|
| Video codec | H.264 High Profile, Level 4.1, yuv420p | Hardware decode on effectively 100% of devices — the only codec with that guarantee |
| Resolution | Fit within 1080×1920 (portrait) / 1920×1080 (landscape), never upscale, rounded to even | 9:16 feed; cap short side at 1080 |
| Framerate | Cap 30 fps (only when source > 30; never resample upward) | Frame-doubling 24→30 wastes bits |
| Rate control | CRF 23, `-maxrate 4M`, tight `-bufsize` (~1–2× maxrate) | Capped-CRF: quality-targeted with a delivery ceiling; tight buffer window = faster playback start on constrained links |
| Startup ramp | First ~2 s encoded at reduced quality (x264 zones, e.g. `zones=0,60,crf=28`) | Initial player buffer fills 2–3× faster → visibly faster first frame; quality recovers before the eye settles (standard short-form trick) |
| Keyframes | Every 2 s | Costs a few % bitrate; makes files HLS-segmentable later with zero re-encode |
| Audio | HE-AAC v1 64–96k stereo 44.1 kHz | AAC-LC 128k spends up to a quarter of a weak connection on phone-speaker audio; HE-AAC support is universal in MP4 |
| Container | MP4, `-movflags +faststart` | Moov up front — the point of the exercise (fMP4/CMAF alternative: Q2) |
| Streams/metadata | `-map 0:v:0 -map 0:a:0?`, `-map_metadata -1` | First video + optional audio only (drops data/subtitle tracks); strips GPS/EXIF |
| Preset | `veryfast` | Sync-path latency; ~15–25% larger than `medium` at same CRF — acceptable, revisit with metrics |

Concrete invocation (target dimensions `{W}×{H}` computed in Rust from rotation-corrected probe output — keeps orientation logic unit-testable, avoids ffmpeg filter-expression bugs; `-r 30` added only when probe shows source fps > 30, since no valid `min(source_fps,30)` expression exists):

```bash
ffmpeg -y -i in.mp4 \
  -map 0:v:0 -map 0:a:0? \
  -c:v libx264 -profile:v high -level 4.1 -pix_fmt yuv420p \
  -crf 23 -preset veryfast -maxrate 4M -bufsize 6M \
  -x264-params "keyint=60:min-keyint=60:zones=0,60,crf=28" \
  -vf scale={W}:{H} \
  ${SRC_FPS_GT_30:+-r 30} \
  -c:a libfdk_aac -profile:a aac_he -b:a 64k -ar 44100 -ac 2 \
  -movflags +faststart \
  -map_metadata -1 \
  out.mp4
```

(Audio note: `libfdk_aac` for HE-AAC requires a non-free ffmpeg build; the Debian package's `aac` encoder is LC-only. Fallback if we stay on stock ffmpeg: AAC-LC at 96k. Verify during implementation.)

Remux variant: `ffmpeg -y -i in.mp4 -c copy -movflags +faststart out.mp4`.

## 5. Phase 1a — Ingest gate for new (videogen) uploads

### 5.1 What it is now

With user uploads gone, this is a **verification gate, not a transcode pipeline**. Provider output is expected to be compliant H.264; the only thing providers don't guarantee is moov placement. Expected steady-state distribution: overwhelmingly passthrough/remux, transcode ~never.

New module `src/transcode.rs`, called from `handler_raw_upload_initial` (`src/routes/duplicate.rs:514`) before the staging upload:

```text
multipart bytes
  → write to temp file (ffmpeg needs seekable input — do NOT pipe)
  → ffprobe (JSON): codec, pix_fmt, profile, dimensions, rotation, fps, audio
  → decide:
      compliant + moov already front  → PASSTHROUGH (no work)
      compliant + moov at end         → REMUX      (-c copy +faststart, ~seconds)
      not compliant                   → TRANSCODE  (canonical v1 — rare fallback)
  → verify output: parse top-level MP4 boxes, assert moov offset < mdat offset
  → extract thumbnail from output file (also fixes current stdin-pipe fragility in
    extract_thumbnail — piped input can't seek, moov-at-end is the pathological case)
  → hand output bytes + probe metadata back to handler
```

**Compliance predicate** (all must hold): container mp4/mov; video h264, yuv420p, profile ≤ High, level ≤ 4.1; display dims fit 1080×1920 / 1920×1080; fps ≤ 30.1; audio absent or AAC.

Why `initial` and not `finalize`: staged artifact is final-form (smaller staging uploads), thumbnail comes from the fixed file, `finalize` stays untouched. AI clips are short (5–10 s), so added latency is small; if that ever changes, the pipeline is a pure `bytes → bytes` function and can move to a background step between `initial` and `finalize`.

### 5.2 Failure policy — never block an upload

Gate failure (corrupt input, ffmpeg error/timeout) → log + metric + **upload the original bytes exactly as today**, with `moov_atom_front = false` recorded so Phase 1b machinery retries later. Per-invocation ffmpeg timeout (120 s) + global concurrency semaphore (start at 2).

### 5.3 Metadata wiring

On success, `handler_raw_finalize` gains a **best-effort** master upsert (same pattern as the videogen hook `on_video_ingested`, `src/jobs/ingest.rs:107-114` — never propagates errors; daily discovery sweep remains the backstop): `moov_atom_front`, `canonical_encoding_version` (`"source"` / `"source-remux-v1"` / `"v1"` by outcome), plus probe fields (width/height/fps/duration/codecs). COALESCE semantics mean later discovery imports don't clobber these.

### 5.4 Config & rollout

`CANONICAL_GATE_ENABLED` (default off), `..._MAX_CONCURRENCY`, `..._TIMEOUT_SECS`. Ship dark → enable → watch metrics (outcome counts, durations, bytes in/out). Verify reverse-proxy timeouts (`deploy/Caddyfile`) tolerate upload + gate.

**Effort: ~1–2 days** (module + fixture-file unit tests + handler integration + flags).

## 6. Phase 1b — Catalog backfill (primary workstream)

The feed's slowness lives in the existing catalog: legacy user uploads (where moov-at-end and heavy bitrates actually are) plus older videogen output. Remux-only, reusing the sharded-fleet machinery from the pHash backfill.

**Per-video job (idempotent):**

1. Select master rows `WHERE moov_atom_front IS NOT TRUE` (skips everything Phase 1a already handled).
2. **Cheap probe first:** two ranged GETs (~16 bytes each) walk the top-level MP4 boxes — box size/type at offset 0 (`ftyp`), then at `ftyp_size` — enough to determine moov position for the overwhelming majority of files. Already-fine videos get `moov_atom_front = true` written with **no download**. While probing, record what's cheap to record (file size; full ffprobe on downloaded files) to build the catalog encoding distribution.
3. Otherwise: download → `ffmpeg -c copy -movflags +faststart` → verify boxes → overwrite object in place (Storj **and** the S3 mirror for SFW) → update master (`moov_atom_front = true`, `canonical_encoding_version = 'faststart-remux-v1'`).

**Why remux only:** lossless, seconds per video, zero quality-regression risk across 600k videos, pHash-stable (decoded frames unchanged — completed pHash backfill and dedup remain valid).

**Heavy-tail follow-up (separate, data-driven decision):** user uploads are dead, so the oversized-file set is frozen and finite. Step 2's distribution tells us exactly how many catalog videos exceed a delivery-hostile bitrate (proposed threshold: > 6 Mbps). Then one decision with real numbers: re-encode those N videos to canonical v1, or accept them. Not part of this approval.

**Sequencing:** SFW bucket first; NSFW after, if in scope (Q5). Fleet: leased jobs + 3-box sharding; job/lease/failure tables already exist.

**Risks to confirm before running (Q4):** in-place overwrite changes object bytes/ETag — confirm nothing references videos by content hash (on-chain refs are by video id — need explicit ack) and CDN/linkshare cache behavior (stale moov-at-end copies just keep today's behavior until TTL; Storj/S3 replaces are atomic, so mid-download clients should be a non-issue — confirming out loud).

**Effort: ~1 week elapsed** (job plumbing + supervised catalog run).

## 7. Phase 2 — Client preloading (mobile team, separate repo)

Faststart makes the first ~1 MB of every file contain the moov + first frames. Mobile should (1) use a 3-player recycle pattern (previous/current/next), (2) prefetch the first 0.5–1 MB of the next 1–2 feed videos with ranged requests, (3) paint the thumbnail instantly while the video loads. This converts "loads fast" into "instant". Handoff note once Phase 1b is underway; not this service's work.

## 8. Phase 3 — ABR/HLS (deferred, criteria-gated)

Re-evaluate only if, after Phases 1–2 are live, stall-rate for the low-bandwidth cohort remains above target. Escalation ladder before full HLS: second 480p progressive rendition → AV1 rendition for capable devices → HLS. The 2 s keyframe cadence (and Q2's container choice) keeps every rung cheap. No work now.

## 9. Pre-implementation verification

1. **Probe ~50 recent videogen outputs** (moov position, codec, dims, bitrate) from master rows. If providers already write faststart, Phase 1a shrinks further and this spec's cost estimate drops; if not, the gate earns its keep from day one. (~10 min against prod.)
2. Confirm `/duplicate` and `/hls/duplicate` receive zero traffic (Q6) — then delete rather than wrap.
3. Verify stock Debian ffmpeg HE-AAC situation (see §4 audio note).

## 10. Open questions for reviewers

| # | Question | Recommendation |
|---|---|---|
| Q1 | CRF 23 vs 26–28 for the (rare) transcode branch and any heavy-tail pass? | 23 + 4M cap; config knob, revisit with data |
| Q2 | Classic faststart MP4 vs fragmented MP4/CMAF as canonical container? fMP4 makes later HLS free (same files, add playlists) but needs an old-Android progressive-playback compatibility check | Classic faststart for v1 (maximum compatibility); revisit at Phase 3 trigger |
| Q3 | Heavy-tail re-encode: agree threshold (> 6 Mbps?) now, decide after Phase 1b produces the count? | Yes — decision deferred to data, threshold agreed up front |
| Q4 | Backfill overwrites objects in place (Storj + S3) — ack that nothing keys on content bytes/ETag; on-chain refs are by video id; CDN TTL acceptable vs explicit purge? | Believe safe; want explicit ack from chain + infra owners |
| Q5 | NSFW bucket in backfill scope? | SFW first regardless; NSFW decision can trail |
| Q6 | Is user upload expected to return to the app? (Decides how much the transcode branch matters, and whether `/duplicate` + `/hls/duplicate` legacy routes can be deleted) | Gate design covers a return either way; delete legacy routes if traffic is zero |

## 11. Out of scope

- HLS/DASH packaging, multi-rendition storage (Phase 3, criteria-gated)
- Mobile player changes (Phase 2, other team/repo)
- Heavy-tail catalog re-encode (separate decision after Phase 1b data)
- The pending `prakash/upload-storage-merge` branch work (Phase 3/swagger/deploy) — Phase 1a touches `duplicate.rs`, so coordinate merge order to avoid conflicts

## 12. Estimated effort

- **Phase 1a (ingest gate):** 1–2 days
- **Phase 1b (catalog backfill):** ~1 week elapsed, mostly supervised fleet runtime
- Heavy-tail pass: sized after 1b data, separately approved

Implementation will follow with a task-level plan (`docs/superpowers/plans/`) once this design is confirmed.
