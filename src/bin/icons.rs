//! Regenerate the Android launcher icons from `barclean.jpg`.
//!
//! # Why this exists rather than five `sips` calls
//!
//! Resizing the source into `mipmap-*/ic_launcher.png` is the easy half and it is not enough. With
//! only legacy bitmaps present, every launcher on Android 8 and later applies its **legacy icon
//! treatment**: it shrinks the bitmap and drops it onto a generated light background, which shows up
//! as a pale border around the artwork. Nothing about the bitmap causes that — supplying an adaptive
//! icon is the only way to stop it.
//!
//! # The geometry, which is the whole point
//!
//! An adaptive icon is a 108×108 dp canvas of which only the centre **72×72 dp** is guaranteed
//! visible; the surrounding 18 dp ring is consumed by the launcher's mask (circle, squircle, rounded
//! square — the shape is the launcher's choice) and by parallax animation.
//!
//! This artwork has ring motifs sitting close to all four corners, so a full-bleed foreground would
//! have them sliced off by a circular mask. Instead the artwork is scaled into the 72 dp safe zone
//! and the background layer is filled with the same black the artwork already sits on. The icon
//! therefore reaches the very edge in black — no pale border, nothing that reads as padding — while
//! every ring survives whatever mask the launcher picks.

use image::{GenericImageView, Rgba, RgbaImage, imageops::FilterType};
use std::path::Path;

/// Density buckets: (directory suffix, legacy icon px, adaptive canvas px at 108 dp).
const DENSITIES: [(&str, u32, u32); 5] = [
    ("mdpi", 48, 108),
    ("hdpi", 72, 162),
    ("xhdpi", 96, 216),
    ("xxhdpi", 144, 324),
    ("xxxhdpi", 192, 432),
];

/// Fraction of the adaptive canvas that is guaranteed visible: 72 of 108 dp.
const SAFE_ZONE: f32 = 72.0 / 108.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_path = root.join("barclean.jpg");
    let res = root.join("android/app/src/main/res");

    let source = image::open(&source_path)?;
    let (sw, sh) = source.dimensions();
    println!("source {} ({sw}x{sh})", source_path.display());

    for (density, legacy_px, canvas_px) in DENSITIES {
        let dir = res.join(format!("mipmap-{density}"));
        std::fs::create_dir_all(&dir)?;

        // Legacy bitmap: full bleed, for pre-Oreo launchers that do no masking at all.
        let legacy = source.resize_exact(legacy_px, legacy_px, FilterType::Lanczos3);
        legacy.save(dir.join("ic_launcher.png"))?;
        legacy.save(dir.join("ic_launcher_round.png"))?;

        // Adaptive foreground: artwork inside the safe zone, transparent beyond it.
        let art_px = ((canvas_px as f32) * SAFE_ZONE).round() as u32;
        let art = source
            .resize_exact(art_px, art_px, FilterType::Lanczos3)
            .to_rgba8();

        let mut canvas = RgbaImage::from_pixel(canvas_px, canvas_px, Rgba([0, 0, 0, 0]));
        let offset = (canvas_px - art_px) / 2;
        for (x, y, px) in art.enumerate_pixels() {
            canvas.put_pixel(x + offset, y + offset, *px);
        }
        canvas.save(dir.join("ic_launcher_foreground.png"))?;

        println!("  {density}: legacy {legacy_px}px, adaptive {canvas_px}px (art {art_px}px)");
    }

    // The adaptive icon itself. Background is the same black the artwork sits on, so the icon
    // reaches the edge of whatever shape the launcher masks it to with no visible seam.
    let anydpi = res.join("mipmap-anydpi-v26");
    std::fs::create_dir_all(&anydpi)?;
    let adaptive = r#"<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@color/ic_launcher_background" />
    <foreground android:drawable="@mipmap/ic_launcher_foreground" />
    <monochrome android:drawable="@mipmap/ic_launcher_foreground" />
</adaptive-icon>
"#;
    std::fs::write(anydpi.join("ic_launcher.xml"), adaptive)?;
    std::fs::write(anydpi.join("ic_launcher_round.xml"), adaptive)?;

    let values = res.join("values");
    std::fs::create_dir_all(&values)?;
    std::fs::write(
        values.join("ic_launcher_background.xml"),
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<resources>\n    \
         <color name=\"ic_launcher_background\">#000000</color>\n</resources>\n",
    )?;

    println!("wrote adaptive icon + background colour");
    Ok(())
}
