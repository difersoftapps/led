//! Catalogo de modos conhecidos da MCU e uma pre-visualizacao aproximada.
//!
//! Do `ledctl.sh`: 1=G 2=B 3=R 4=G+B 5=R+G 6=B+R 7=rainbow 8=scroll 9=OFF e
//! 17..=24 = versoes "breathing". O menu original so oferecia 17, 18, 19, 23 e 24;
//! 20, 21 e 22 (ciano/amarelo/magenta respirando) sao uma DEDUCAO (base + 16) e por
//! isso aparecem marcados com `verified: false`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Rgb(u8, u8, u8),
    Rainbow,
    Scroll,
}

#[derive(Clone, Copy, Debug)]
pub struct Effect {
    pub label: &'static str,
    pub mode: u8,
    /// `true` = presente no menu original do dArkOS (comprovadamente aceito pela MCU).
    pub verified: bool,
}

const fn e(label: &'static str, mode: u8, verified: bool) -> Effect {
    Effect {
        label,
        mode,
        verified,
    }
}

pub const SOLID: [Effect; 8] = [
    e("GREEN", 1, true),
    e("BLUE", 2, true),
    e("RED", 3, true),
    e("CYAN", 4, true),
    e("YELLOW", 5, true),
    e("MAGENTA", 6, true),
    e("RAINBOW", 7, true),
    e("SCROLL", 8, true),
];

pub const BREATHING: [Effect; 8] = [
    e("GREEN", 17, true),
    e("BLUE", 18, true),
    e("RED", 19, true),
    e("CYAN", 20, false),
    e("YELLOW", 21, false),
    e("MAGENTA", 22, false),
    e("RAINBOW", 23, true),
    e("CYCLE", 24, true),
];

/// Decodifica um byte de modo em (tipo, respirando?). `None` para OFF (9) e
/// bytes desconhecidos.
pub fn decode(mode: u8) -> Option<(Kind, bool)> {
    let breathing = (17..=24).contains(&mode);
    let base = match mode {
        1..=8 => mode,
        17..=24 => mode - 16,
        _ => return None,
    };
    let kind = match base {
        1 => Kind::Rgb(0, 255, 0),
        2 => Kind::Rgb(0, 0, 255),
        3 => Kind::Rgb(255, 0, 0),
        4 => Kind::Rgb(0, 255, 255),
        5 => Kind::Rgb(255, 255, 0),
        6 => Kind::Rgb(255, 0, 255),
        7 => Kind::Rainbow,
        _ => Kind::Scroll,
    };
    Some((kind, breathing))
}

/// Cor aproximada do LED `led` (de `n`) no instante `t` (s). So para a UI: o
/// comportamento real de rainbow/scroll/breathing e decidido pela firmware.
pub fn preview(mode: u8, led: usize, n: usize, t: f32) -> (u8, u8, u8) {
    let Some((kind, breathing)) = decode(mode) else {
        return (40, 40, 48);
    };
    let n = n.max(1) as f32;
    let (r, g, b) = match kind {
        Kind::Rgb(r, g, b) => (r, g, b),
        Kind::Rainbow => hsv((led as f32 / n * 360.0 + t * 30.0) % 360.0),
        Kind::Scroll => hsv((t * 120.0 - led as f32 * 360.0 / n).rem_euclid(360.0)),
    };
    let k = if breathing {
        0.12 + 0.88 * (0.5 + 0.5 * (t * std::f32::consts::TAU / 3.0).sin())
    } else {
        1.0
    };
    (
        (r as f32 * k) as u8,
        (g as f32 * k) as u8,
        (b as f32 * k) as u8,
    )
}

fn hsv(h: f32) -> (u8, u8, u8) {
    let x = 1.0 - ((h / 60.0) % 2.0 - 1.0).abs();
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_basico() {
        assert_eq!(decode(3), Some((Kind::Rgb(255, 0, 0), false)));
        assert_eq!(decode(19), Some((Kind::Rgb(255, 0, 0), true)));
        assert_eq!(decode(24), Some((Kind::Scroll, true)));
        assert_eq!(decode(9), None); // OFF
        assert_eq!(decode(0), None);
        assert_eq!(decode(200), None);
    }

    #[test]
    fn catalogo_so_tem_modos_decodificaveis() {
        for ef in SOLID.iter().chain(BREATHING.iter()) {
            assert!(decode(ef.mode).is_some(), "{}", ef.mode);
        }
        // breathing = base + 16
        for (s, b) in SOLID.iter().zip(BREATHING.iter()) {
            assert_eq!(s.mode + 16, b.mode);
        }
    }

    #[test]
    fn preview_fixo_e_respirando() {
        assert_eq!(preview(3, 0, 6, 0.0), (255, 0, 0));
        let (r, _, _) = preview(19, 0, 6, 0.75); // pico do seno
        assert!(r > 200);
        let (r, _, _) = preview(19, 0, 6, 2.25); // vale
        assert!(r < 60);
    }
}
