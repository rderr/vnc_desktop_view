//! Decodes an Annex B .h264 file and saves the last frame as a PNG.
//! Usage: cargo run --release --example verify -- IN.h264 OUT.png

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use openh264::nal_units;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1])?;
    let mut decoder = Decoder::new()?;
    let (mut frames, mut last) = (0, None);
    for nal in nal_units(&data) {
        if let Some(yuv) = decoder.decode(nal)? {
            let (w, h) = yuv.dimensions();
            let mut rgb = vec![0u8; w * h * 3];
            yuv.write_rgb8(&mut rgb);
            last = Some((w, h, rgb));
            frames += 1;
        }
    }
    let (w, h, rgb) = last.ok_or_else(|| anyhow::anyhow!("no frames decoded"))?;
    let mut enc = png::Encoder::new(std::fs::File::create(&args[2])?, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.write_header()?.write_image_data(&rgb)?;
    println!("decoded {frames} frames, last {w}x{h} -> {}", args[2]);
    Ok(())
}
