//! Glyph advance widths (units of 1/1000 em) for the PDF standard-14
//! fonts, indexed by WinAnsi byte. Sourced from the Adobe AFM metrics
//! PDFBox loads for `PDType1Font`; used by `misc/add-page-numbers` for
//! text-width and centering math. Bytes WinAnsi cannot encode carry the
//! width of `?`, matching how the encoder substitutes it.

/// Helvetica widths; `Ascender`/`Descender` descriptor values live in
/// `misc.rs` next to the font selection.
pub(crate) const HELVETICA_W: [u16; 256] = [
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 355, 556, 556, 889,
    667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667,
    556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469,
    556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556,
    333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, 556, 556, 556, 556, 222, 556,
    333, 1000, 556, 556, 333, 1000, 556, 333, 1000, 556, 556, 556, 222, 222, 333, 333, 350, 556,
    1000, 333, 556, 556, 333, 944, 556, 556, 556, 278, 333, 556, 556, 556, 556, 556, 556, 333, 556,
    370, 556, 556, 333, 556, 333, 556, 556, 556, 556, 333, 556, 537, 278, 333, 556, 365, 556, 556,
    556, 556, 611, 556, 556, 556, 556, 556, 556, 1000, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 556, 778, 556, 556, 556, 556, 556, 556, 611, 556, 556, 556,
    556, 556, 556, 889, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 611, 556, 556, 556, 556, 556, 556, 556,
];

pub(crate) const TIMES_W: [u16; 256] = [
    444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444,
    444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 250, 333, 408, 500, 500, 833,
    778, 180, 333, 333, 500, 564, 250, 333, 250, 278, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    500, 278, 278, 564, 564, 564, 444, 921, 722, 667, 667, 722, 611, 556, 722, 722, 333, 389, 722,
    611, 889, 722, 722, 556, 722, 667, 556, 611, 722, 722, 944, 722, 722, 611, 333, 278, 333, 469,
    500, 333, 444, 500, 444, 500, 444, 333, 500, 500, 278, 278, 500, 278, 778, 500, 500, 500, 500,
    333, 389, 278, 500, 500, 722, 500, 500, 444, 480, 200, 480, 541, 444, 444, 444, 333, 500, 444,
    1000, 500, 500, 333, 1000, 444, 333, 889, 444, 444, 444, 444, 333, 333, 444, 444, 350, 500,
    1000, 333, 444, 444, 333, 722, 444, 444, 444, 250, 333, 500, 500, 500, 500, 444, 500, 333, 444,
    276, 500, 444, 333, 444, 333, 444, 444, 444, 444, 333, 444, 453, 250, 333, 444, 310, 500, 444,
    444, 444, 444, 444, 444, 444, 444, 444, 444, 889, 444, 444, 444, 444, 444, 444, 444, 444, 444,
    444, 444, 444, 444, 444, 444, 444, 444, 722, 444, 444, 444, 444, 444, 444, 500, 444, 444, 444,
    444, 444, 444, 667, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444, 444,
    444, 444, 500, 444, 444, 444, 444, 444, 444, 444,
];

pub(crate) const COURIER_W: [u16; 256] = [
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600, 600,
    600, 600, 600, 600, 600, 600, 600, 600, 600,
];

/// Standard-14 font descriptor metrics: `(ascent, descent)` in glyph
/// space, matching the AFM `Ascender`/`Descender` values PDFBox exposes
/// through `PDFontDescriptor`.
pub(crate) fn ascent_descent(base_font: &str) -> (f64, f64) {
    match base_font {
        "Times-Roman" => (683.0, -217.0),
        "Courier" => (629.0, -157.0),
        _ => (718.0, -207.0),
    }
}

/// `PDType1Font.getStringWidth(text) / 1000`: sum of per-glyph advances
/// over WinAnsi-encoded bytes (see `winansi_safe` in `handlers/misc.rs`).
pub(crate) fn string_width(base_font: &str, encoded: &[u8]) -> f64 {
    let table = match base_font {
        "Times-Roman" => &TIMES_W,
        "Courier" => &COURIER_W,
        _ => &HELVETICA_W,
    };
    encoded.iter().map(|&b| f64::from(table[b as usize])).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn courier_is_fixed_width() {
        // Courier's AFM advance is 600/1000 em for every glyph.
        assert_eq!(string_width("Courier", b"iii"), 1800.0);
        assert_eq!(string_width("Courier", b"WWW"), 1800.0);
        assert_eq!(string_width("Courier", b""), 0.0);
    }

    #[test]
    fn helvetica_proportional_widths() {
        // 'W' is wider than 'i' in Helvetica; the sum must reflect that.
        let wide = string_width("Helvetica", b"W");
        let narrow = string_width("Helvetica", b"i");
        assert!(wide > narrow);
        assert_eq!(wide, 944.0);
        assert_eq!(narrow, 222.0);
    }

    #[test]
    fn times_matches_adobe_afm() {
        // Times-Roman 'H' = 722, 'e' = 444, space = 250 (units/1000 em).
        assert_eq!(string_width("Times-Roman", b"He "), 722.0 + 444.0 + 250.0);
    }

    #[test]
    fn unknown_font_falls_back_to_helvetica() {
        assert_eq!(
            string_width("ZapfDingbats", b"W"),
            string_width("Helvetica", b"W")
        );
        assert_eq!(string_width("", b"W"), string_width("Helvetica", b"W"));
    }

    #[test]
    fn every_table_has_256_entries() {
        assert_eq!(HELVETICA_W.len(), 256);
        assert_eq!(TIMES_W.len(), 256);
        assert_eq!(COURIER_W.len(), 256);
        // A full byte range must not panic.
        let all: Vec<u8> = (0..=255).collect();
        for font in ["Helvetica", "Times-Roman", "Courier"] {
            assert!(string_width(font, &all) > 0.0);
        }
    }

    #[test]
    fn ascent_descent_matches_pdfbox_descriptor() {
        assert_eq!(ascent_descent("Times-Roman"), (683.0, -217.0));
        assert_eq!(ascent_descent("Courier"), (629.0, -157.0));
        // Helvetica and every other standard-14 font share the default.
        assert_eq!(ascent_descent("Helvetica"), (718.0, -207.0));
        assert_eq!(ascent_descent("Courier-Bold"), (718.0, -207.0));
    }
}
