//! Shared colour policy. Hosts decide where colours are stored and applied.
pub(crate) fn luminance(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

pub(crate) fn readable_ink(ink: [f32; 3], paper: [f32; 3]) -> [f32; 3] {
    if (luminance(ink) - luminance(paper)).abs() >= 0.45 {
        return ink;
    }
    let inverted = ink.map(|channel| 1.0 - channel);
    if (luminance(inverted) - luminance(paper)).abs() > (luminance(ink) - luminance(paper)).abs() {
        inverted
    } else {
        ink
    }
}
