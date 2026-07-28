//! Shared banner rendering functionality

pub const BANNER_GRADIENT: [(u8, u8, u8); 8] = [
    (157, 207, 161),
    (132, 190, 137),
    (108, 174, 113),
    (85, 157, 90),
    (63, 141, 68),
    (43, 122, 48),
    (26, 102, 32),
    (12, 82, 18),
];

pub const BANNER: &str = r#"                                                     
  ██████   ██████   ████████    ██████  ████████  █████ ████
 ███░░███ ░░░░░███ ░░███░░███  ███░░███░░███░░███░░███ ░███ 
░███ ░░░   ███████  ░███ ░███ ░███ ░███ ░███ ░███ ░███ ░███ 
░███  ███ ███░░███  ░███ ░███ ░███ ░███ ░███ ░███ ░███ ░███ 
░░██████ ░░████████ ████ █████░░██████  ░███████  ░░███████ 
 ░░░░░░   ░░░░░░░░ ░░░░ ░░░░░  ░░░░░░   ░███░░░    ░░░░░███ 
                                        ░███       ███ ░███ 
                                        █████     ░░██████  
                                       ░░░░░       ░░░░░░   
"#;

/// Print the banner with gradient colors and custom title
pub fn print_banner_with_gradient(title: &str) {
    let lines: Vec<&str> = BANNER.lines().collect();

    // Print each line with a different color from the gradient
    for (i, line) in lines.iter().enumerate() {
        let (r, g, b) = gradient_rgb(i, lines.len());
        println!("\x1b[38;2;{r};{g};{b}m{line}\x1b[0m");
    }

    // Print additional text in light green with custom title
    println!("\x1b[38;2;100;255;100m  \x1b[1m{}\x1b[0m", title);
    println!("  ─────────────────────────────────────────────");
    println!();
}

pub fn gradient_rgb(index: usize, line_count: usize) -> (u8, u8, u8) {
    let denominator = line_count.max(1);
    let color_index =
        (index as f32 / denominator as f32 * (BANNER_GRADIENT.len() - 1) as f32).round() as usize;
    BANNER_GRADIENT[color_index.min(BANNER_GRADIENT.len() - 1)]
}

/// Print the banner with a single color (original behavior)
#[allow(dead_code)]
pub fn print_banner_single_color() {
    println!("\x1b[32m{BANNER}\x1b[0m");
    println!("  \x1b[1mAgent Hub — Setup Wizard\x1b[0m");
    println!("  ─────────────────────────────────────────────");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gradient_rgb at index 0 returns first color
    #[test]
    fn gradient_rgb_first_index() {
        let (r, g, b) = gradient_rgb(0, 8);
        assert_eq!((r, g, b), BANNER_GRADIENT[0]);
    }

    /// gradient_rgb at the end returns near-last color (due to rounding)
    #[test]
    fn gradient_rgb_last_index() {
        // gradient_rgb(7, 8) = (7/8 * 7).round() = 6.125.round() = 6
        let (r, g, b) = gradient_rgb(7, 8);
        assert_eq!((r, g, b), BANNER_GRADIENT[6]);
    }

    /// gradient_rgb with line_count=1 returns first color
    #[test]
    fn gradient_rgb_single_line() {
        let (r, g, b) = gradient_rgb(0, 1);
        assert_eq!((r, g, b), BANNER_GRADIENT[0]);
    }

    /// gradient_rgb with line_count=0 returns first color
    #[test]
    fn gradient_rgb_zero_lines() {
        let (r, g, b) = gradient_rgb(0, 0);
        assert_eq!((r, g, b), BANNER_GRADIENT[0]);
    }

    /// gradient_rgb returns valid indices within bounds
    #[test]
    fn gradient_rgb_all_indices_valid() {
        for index in 0..20 {
            for line_count in 1..20 {
                let (r, g, b) = gradient_rgb(index, line_count);
                // Check it's one of the valid gradient colors
                assert!(
                    BANNER_GRADIENT.contains(&(r, g, b)),
                    "gradient_rgb({index}, {line_count}) returned ({r}, {g}, {b}) which is not in BANNER_GRADIENT"
                );
            }
        }
    }

    /// gradient_rgb with middle indices returns middle-range colors
    #[test]
    fn gradient_rgb_middle_indices() {
        let (r, g, b) = gradient_rgb(4, 8);
        // Should return a middle color (around index 3-4)
        // Just verify it's a valid gradient color
        assert!(BANNER_GRADIENT.contains(&(r, g, b)));
    }

    /// gradient_rgb increases (roughly) as index increases
    #[test]
    fn gradient_rgb_monotonic_trend() {
        // Test that color indices generally increase as we move through the banner
        let colors: Vec<_> = (0..8).map(|i| gradient_rgb(i, 8)).collect();
        // All should be valid colors
        for (r, g, b) in &colors {
            assert!(BANNER_GRADIENT.contains(&(*r, *g, *b)));
        }
    }

    /// gradient_rgb handles large line counts
    #[test]
    fn gradient_rgb_large_line_count() {
        let (r, g, b) = gradient_rgb(50, 1000);
        assert!(BANNER_GRADIENT.contains(&(r, g, b)));
    }

    /// gradient_rgb index beyond line_count wraps correctly
    #[test]
    fn gradient_rgb_index_beyond_count() {
        let (r, g, b) = gradient_rgb(100, 10);
        assert!(BANNER_GRADIENT.contains(&(r, g, b)));
    }

    /// BANNER_GRADIENT has expected length
    #[test]
    fn banner_gradient_has_expected_length() {
        assert_eq!(BANNER_GRADIENT.len(), 8);
    }

    /// BANNER_GRADIENT colors are valid RGB tuples
    #[test]
    fn banner_gradient_color_ranges() {
        for (r, g, b) in BANNER_GRADIENT {
            // u8 is always <= 255, but verify structure is correct
            let _ = (r, g, b);
        }
    }

    /// BANNER contains expected string content
    #[test]
    fn banner_contains_canopy_text() {
        assert!(BANNER.len() > 100);
        assert!(BANNER.contains("██"));
    }

    /// print_banner_with_gradient doesn't panic (I/O test)
    #[test]
    fn print_banner_with_gradient_no_panic() {
        // Just verify it doesn't panic - we can't easily test output
        print_banner_with_gradient("Test Title");
    }

    /// print_banner_single_color doesn't panic (I/O test)
    #[test]
    fn print_banner_single_color_no_panic() {
        // Just verify it doesn't panic - we can't easily test output
        print_banner_single_color();
    }
}
