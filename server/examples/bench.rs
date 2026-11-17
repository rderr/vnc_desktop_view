//! Times H.264 encoding of a real desktop image with a changing region, per usage type.
use openh264::OpenH264API;
use openh264::encoder::*;
use openh264::formats::{BgraSliceU8, YUVBuffer};
use std::time::Instant;
fn main() -> anyhow::Result<()> {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&std::env::args().nth(1).unwrap())?));
    let mut reader = dec.read_info()?;
    let mut rgb = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut rgb)?;
    let (w, h) = (info.width as usize, info.height as usize);
    let base: Vec<u8> = rgb.chunks(3).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    for (name, usage) in [("screen", UsageType::ScreenContentRealTime), ("camera", UsageType::CameraVideoRealTime)] {
        for complexity in [Complexity::Low, Complexity::Medium] {
            let cfg = EncoderConfig::new().usage_type(usage).rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(8_000_000)).max_frame_rate(FrameRate::from_hz(60.0)).complexity(complexity)
                .adaptive_quantization(false).background_detection(false);
            let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg)?;
            let mut yuv = YUVBuffer::new(w, h);
            let mut bgra = base.clone();
            let (mut times, mut bytes) = (Vec::new(), 0);
            for i in 0..40usize {
                // a 800x200 band changes colour + some moving text-like stripes
                for y in 300..500 { for x in 100..900 { let o = (y * w + x) * 4; bgra[o] = (i * 7) as u8; bgra[o + 1] = ((x + i * 13) % 256) as u8; bgra[o + 2] = 200; } }
                yuv.read_bgra8(BgraSliceU8::new(&bgra, (w, h)));
                let t = Instant::now(); bytes += enc.encode(&yuv)?.to_vec().len(); times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let first = times[0]; let rest: f64 = times[1..].iter().sum::<f64>() / (times.len() - 1) as f64;
            println!("{name:6} {complexity:?}: first {first:.0} ms, then {rest:.1} ms/frame, {} KB total", bytes / 1000);
        }
    }
    Ok(())
}
