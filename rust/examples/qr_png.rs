fn main() {
    let data = std::env::args().nth(1).expect("data");
    let path = std::env::args().nth(2).expect("out.png");
    let code = qrcode::QrCode::new(data.into_bytes()).unwrap();
    let w = code.width();
    let colors = code.to_colors();
    let qz = 4usize;
    let size = ((w + 2 * qz) * 8) as u32;
    let mut img = image::GrayImage::from_pixel(size, size, image::Luma([255u8]));
    for y in 0..w {
        for x in 0..w {
            let dark = colors[y * w + x].select(true, false);
            if dark {
                for dy in 0..8usize {
                    for dx in 0..8usize {
                        img.put_pixel(
                            ((x + qz) * 8 + dx) as u32,
                            ((y + qz) * 8 + dy) as u32,
                            image::Luma([0u8]),
                        );
                    }
                }
            }
        }
    }
    img.save(&path).unwrap();
}
