# barclean

**Recovers barcodes that people ruined with logos.**

Marketers paste a logo over the middle of a QR code and let Reed–Solomon absorb the damage. It
usually works — in the studio. It also spends the symbol's entire error budget on self-inflicted
damage, leaving nothing for glare, creases, a crumpled receipt, bad printing, or a bad angle. That's
why branded codes fail in the wild while the plain ones next to them scan first time.

barclean gets those symbols back, and it does it by exploiting something every mainstream decoder
throws away.

---

## The lever

Reed–Solomon corrects `t` **errors** — unknown position, unknown value — where `2t ≤ n−k`.

It corrects `e` **erasures** — *known position*, unknown value — where `e ≤ n−k`.

**Knowing where the damage is is worth exactly twice the correction power.** A logo is damage whose
position is, in principle, perfectly knowable. No shipping decoder uses this, because they all
binarize before detection: by the time anything knows where a module sits, the greyscale and colour
that would have revealed the logo are gone, and the covered modules arrive as confident, wrong bits.

## The trick that makes it work without looking at pixels

A Reed–Solomon block that decodes has *proved*, algebraically, which of its codewords were wrong.
That's a damage map with no false positives and no image heuristics behind it.

The catch is that RS is all-or-nothing, so the map only exists for blocks that didn't need it.

QR's interleaving is what turns that into a mechanism. Consecutive codewords are scattered across
every block in rotation, so a contiguous logo is spread evenly rather than concentrated. Blocks that
survive report exactly which codewords were damaged; those map back to damaged *modules*; and because
an occlusion is a **blob**, that outlines where the logo is — including over the blocks that failed.
Declare those codewords as erasures, retry at double the correction power, and blocks start coming
back. Every rescued block adds its own exact damage to the evidence, which sharpens the fit, which
rescues the next one.

```
  decode blocks ──> survivors prove damaged codewords
        ^                        │
        │                        v
  erasures for            map to modules, fit the blob
  failed blocks                  │
        ^                        v
        └──── which codewords does the blob cover? ────┘
```

No image statistics anywhere in that loop. It's derived from the algebra of the code itself, which
makes it strongest exactly where pixel heuristics are weakest — bad light, heavy blur, or a logo
that's flat and neutral and looks like legitimate module content to every per-pixel measurement.

## What it actually recovers

Measured, not asserted. `cargo test --test bootstrap_recovers_occluded_symbols -- --nocapture`:

| ECC | occlusion | plain decode | barclean |
|-----|-----------|--------------|----------|
| M   | 13–16%    | 3/4 blocks   | **4/4 — full recovery** |
| Q   | 21–22%    | 7/8 blocks   | **8/8 — full recovery** |
| Q   | 23–24%    | 6/8 blocks   | **8/8 — full recovery** |
| H   | 26–27%    | 9/11 blocks  | **11/11 — full recovery** |
| H   | 28%       | 6/11 blocks  | **11/11 — full recovery** |
| H   | 29–30%    | 4/11 blocks  | **11/11 — full recovery** |
| H   | 31–32%    | 2/11 blocks  | 8/11 (partial) |

At ECC-H with 30% of the symbol covered, plain decoding gets 4 of 11 blocks — a dead symbol.
barclean recovers all eleven. The full-decode ceiling moves 25%→30% at H, 20%→24% at Q, 12%→16% at M.

Two properties are non-negotiable and tested across every ECC level and every occlusion step:

- **It never loses ground.** The floor is always plain decoding's result.
- **Clean symbols are untouched.** No erasures spent, nothing changed.

Caveat on reading those percentages: occlusion is applied at module level, so the imaging pipeline
isn't in play, and the occluding square only damages modules that disagree with it — roughly half the
covered area. Real-camera figures will be lower. The honest limit is the first round: if *no* block
decodes, there's no evidence to bootstrap from, and that's where image-based confidence has to take
over.

## Scope

Four 2D symbologies: **QR, Aztec, PDF417, DataMatrix**. These are the formats with enough error
correction for the erasure lever to buy anything, and the ones people actually deface. 1D symbols
have thin ECC and rarely wear a centre logo.

Aztec is the awkward one and worth stating plainly: its finder **is** the centre bullseye, so a
centre logo is a *detection* failure, not a correction failure. No amount of erasure decoding helps
a symbol that was never located.

## Status

Early. Version stays at `0.0.0` until it's been beta tested.

**Working**
- Erasure-aware Reed–Solomon (`decode_with_erasures`) over QR and DataMatrix fields
- Per-block decoding that survives partial failure, with the interleave map and codeword provenance
- The bootstrap loop, validated end-to-end on real encoded symbols
- Corpus generator: encode → logo composite → realistic degradation, with pristine ground truth
- Selectable lens picker — annotates each physical camera with predicted px/module
- Android app on device: live camera preview, decoding every frame

**Not yet**
- The camera path still runs *stock* decoding; the bootstrap machinery isn't wired to it yet
- Confidence sampling from pixels (the fallback for when no block decodes)
- Exact reconstruction — re-rendering the pristine symbol from the corrected codewords
- Aztec, PDF417 and DataMatrix beyond the shared RS layer

## Building

Desktop, algorithm only — no windowing toolchain in the graph:

```sh
cargo test --no-default-features
```

Android (needs `cargo-ndk`, an NDK, and a device):

```sh
./build-android.sh install
```

`ANDROID_HOME` defaults to `~/android-sdk`. Two alignments both have to be right on 16 KB-page
devices (anything Android 15+): the `.so`'s own LOAD segments, handled by a link flag in
`build-android.sh`, and the library's byte offset *inside the APK* — see the comment in
`android/app/build.gradle`, which cost an afternoon to track down.

## Built on

- [rxing](https://github.com/rxing-core/rxing) — the Rust ZXing port, forked at
  [nickspiker/rxing](https://github.com/nickspiker/rxing) to add erasure-aware Reed–Solomon,
  per-block partial decoding, and codeword provenance. The RS work is cleanly separable and
  upstreamable.
- [fluor](https://github.com/nickspiker/fluor) — CPU softbuffer GUI compositor. One `FluorApp` runs
  on both the desktop shell and the Android shell.

## Licence

MIT OR Apache-2.0.
