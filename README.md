# barclean

**Photograph a damaged barcode. Get back a clean one.**

Folded, smudged, scuffed, badly printed — or with a logo pasted over the middle. barclean recovers
the data, rebuilds the symbol that was actually there, and saves it as a PNG.

Android app, Rust core, four symbologies: **QR, Aztec, DataMatrix, PDF417**.

---

## Two ideas

### 1. Knowing *where* damage is doubles what Reed–Solomon can fix

RS corrects `t` **errors** — unknown position, unknown value — where `2t ≤ n−k`. It corrects `e`
**erasures** — *known position*, unknown value — where `e ≤ n−k`. Twice as many.

A logo is damage whose position is, in principle, perfectly knowable. No shipping decoder exploits
this, because they all binarize before detection: by the time anything knows where a module sits,
the greyscale and colour that would have revealed the logo are gone, and the covered modules arrive
as confident, wrong bits.

### 2. A partly-decoded symbol tells you where the damage is — with no image processing at all

A Reed–Solomon block that decodes has *proved*, algebraically, which of its codewords were wrong.
No false positives, no heuristics. The catch is that RS is all-or-nothing, so that map only exists
for blocks that didn't need it.

QR's interleaving turns that into a mechanism. Consecutive codewords are scattered across every
block in rotation, so a contiguous logo is spread evenly rather than concentrated. Surviving blocks
report exactly which codewords were damaged; those map back to damaged **modules**; and because an
occlusion is a *blob*, that outlines where the logo is — including over the blocks that failed.
Declare those as erasures, retry at double the correction power, and blocks start coming back. Every
rescued block adds its own exact damage to the evidence, sharpening the fit, rescuing the next one.

```
  decode blocks ──> survivors prove damaged codewords
        ^                        │
        │                        v
  erasures for            map to modules, fit the blob
  failed blocks                  │
        ^                        v
        └──── which codewords does the blob cover? ────┘
```

Strongest exactly where pixel heuristics are weakest: bad light, heavy blur, or a logo that's flat
and neutral and looks like legitimate module content to every per-pixel measurement.

## What it recovers

Measured, not asserted — `cargo test --test bootstrap_recovers_occluded_symbols -- --nocapture`:

| ECC | occlusion | plain decode | barclean |
|-----|-----------|--------------|----------|
| M   | 13–16%    | 3/4 blocks   | **4/4 — full recovery** |
| Q   | 19–25%    | 2–7/8 blocks | **8/8 — full recovery** |
| H   | 26–30%    | 4–9/11 blocks| **11/11 — full recovery** |
| H   | 31–32%    | 2/11 blocks  | 8/11 (partial) |

At ECC-H with 30% of the symbol covered, plain decoding gets 4 of 11 blocks — a dead symbol.
barclean recovers all eleven. The full-decode ceiling moves 25%→30% at H, 20%→24% at Q, 12%→16% at M.

Two properties are non-negotiable, tested at every ECC level and every occlusion step:

- **It never loses ground.** The floor is always plain decoding's result.
- **Clean symbols are untouched.** No erasures spent, nothing changed.

Occlusion is applied at module level in those figures, so the imaging pipeline isn't in play, and an
opaque square only damages modules that disagree with it — roughly half the covered area.
Real-camera numbers are lower. The honest limit is the first round: if *no* block decodes there's no
evidence to bootstrap from, which is where image-based confidence would have to take over.

## Restoration, not re-creation

The output is rebuilt from the **corrected codewords**, not by re-encoding the payload.

That distinction is the whole thing. Encoders differ in how they segment a payload across numeric,
alphanumeric and byte modes, where they place mode switches, how they pad, which ECI they declare.
Two encoders given identical text routinely emit different symbols. A "cleaned" code that scans to
the right string but is structurally a different symbol is a re-creation, not a restoration — and it
won't look like the code you photographed.

So the tests compare **module by module** against the original, not payload against payload.

| | detect | recover | restore | bootstrap |
|---|---|---|---|---|
| **QR** | ✓ | ✓ | bit-exact | ✓ |
| **Aztec** | ✓ | ✓ | bit-exact | — single RS block |
| **DataMatrix** | ✓ | ✓ | bit-exact | ✓ above 24×24 |
| **PDF417** | ✓ | ✓ | re-encoded + verified | — single RS block |

Aztec and PDF417 carry one Reed–Solomon block, so there are no survivors to bootstrap from — they
decode or they don't. They still get everything that follows a successful decode, which covers the
damage people actually bring: folds, smudges, scuffs, bad printing.

PDF417 is the one remaining gap: its corrected codewords sit inside a private call chain in the
scanning decoder, so it is re-encoded from the payload for now — and every re-encode is verified by
decoding it again and comparing payloads before it can be saved. A restoration that doesn't scan is
useless; one that scans to *something else* is dangerous.

## The app

Point it at a code. It freezes on a successful scan and shows what it did:

- **green** — light module, the scan already had it right
- **blue** — dark module, already right
- **yellow** — light module, **recovered**
- **red** — dark module, **recovered**

A logo's footprint shows up as a solid yellow-and-red patch, so "this was reconstructed" is something
you can see rather than take on faith. Where no comparison is possible the symbol is drawn plainly in
black and white, and says so — it never colours an uncompared rebuild as if it matched.

**Save** writes a black-and-white PNG to `Pictures/barclean` as `2026-08-10 14:33:48.png`, with the
quiet zone the symbology needs. **Cancel** returns to the camera.

Other things it does:

- **A button per physical camera**, annotated with the pixels-per-module each would deliver on the
  symbol in frame, and labelled by true angular magnification (`f / sensor_width`) — so a 0.5×
  ultra-wide reads as 0.5×, not the 0.3× a naive focal-length ratio gives. Selection is yours; the
  app never switches lenses on you.
- **Inverted codes.** Light-on-dark symbols are retried inverted, and the export preserves the
  polarity it found — a white-on-black sign is restored white-on-black.
- **Mirrored codes.** A QR's three finder patterns are symmetric about the diagonal, so symbols are
  routinely sampled transposed; both orientations are tried.

## Building

Algorithm only, no windowing toolchain in the graph:

```sh
cargo test --no-default-features
```

Android (needs `cargo-ndk`, an NDK, and a device):

```sh
./build-android.sh install
```

`ANDROID_HOME` defaults to `~/android-sdk`. Verified on a Pixel 8 Pro and a Galaxy S24.

Two alignments both have to be right on 16 KB-page devices (Android 15+): the `.so`'s own LOAD
segments, handled by a link flag in `build-android.sh`, and the library's byte offset *inside the
APK* — see the comment in `android/app/build.gradle`.

## Built on

- [rxing](https://github.com/rxing-core/rxing) — the Rust ZXing port, forked at
  [nickspiker/rxing](https://github.com/nickspiker/rxing) (branch `barclean`) to add erasure-aware
  Reed–Solomon, per-block partial decoding, and codeword-to-module provenance for QR, Aztec and
  DataMatrix. All additive; the RS work is cleanly upstreamable.
- [fluor](https://github.com/nickspiker/fluor) — CPU softbuffer GUI compositor. One `FluorApp` runs
  on both the desktop shell and the Android shell.

## Status

Early — version stays at `0.0.0` until it has been beta tested.

## Licence

MIT OR Apache-2.0.
