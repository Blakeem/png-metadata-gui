//! One-off tool: SVG -> PNGs at icon sizes -> .ico (PNG-compressed entries).
//! Usage: icon-forge <input.svg> <out_dir> [name]

use resvg::tiny_skia;
use resvg::usvg;

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let input = args.first().expect("usage: icon-forge <input.svg> <out_dir> [name]");
    let out_dir = args.get(1).expect("usage: icon-forge <input.svg> <out_dir> [name]");
    let name = args.get(2).map(String::as_str).unwrap_or("icon");
    std::fs::create_dir_all(out_dir).expect("create out dir");

    let svg_data = std::fs::read(input).expect("read svg");
    let mut options = usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(&svg_data, &options).expect("parse svg");
    let source_size = tree.size();

    let mut ico_entries: Vec<(u32, Vec<u8>)> = Vec::new();
    for size in SIZES {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
        let scale = size as f32 / source_size.width();
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let png = pixmap.encode_png().expect("encode png");
        let path = format!("{out_dir}/{name}-{size}.png");
        std::fs::write(&path, &png).expect("write png");
        ico_entries.push((size, png));
    }

    // ICO container: ICONDIR + ICONDIRENTRY per image + raw PNG blobs.
    let mut ico: Vec<u8> = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    ico.extend_from_slice(&(ico_entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * ico_entries.len() as u32;
    for (size, png) in &ico_entries {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        ico.push(dim); // width (0 = 256)
        ico.push(dim); // height
        ico.push(0); // palette colors
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &ico_entries {
        ico.extend_from_slice(png);
    }
    let ico_path = format!("{out_dir}/{name}.ico");
    std::fs::write(&ico_path, &ico).expect("write ico");
    println!("wrote {} PNGs + {ico_path}", SIZES.len());
}
