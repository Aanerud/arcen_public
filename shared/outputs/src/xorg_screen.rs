//! The per-desktop screen lines of a dedicated session Xorg configuration:
//! its depth and, for a single head, a `Virtual` size large enough for the
//! served display.
//!
//! The template is root-owned and operator-editable, so only its packaged
//! shape is rewritten: exactly one `DefaultDepth` line and exactly one
//! `Depth` line inside the `Display` subsection. Anything else keeps the
//! template's own depth, and the live root depth the server reports then
//! decides what the desktop can serve.
//!
//! No I/O happens here: the Linux launcher reads the template and starts the
//! server.

use arcen_media::video::DesktopDepth;

/// What one session's screen needs from the template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionScreen {
    /// The depth this desktop is created at; `None` keeps the template's.
    pub depth: Option<DesktopDepth>,
    /// The served display's size, whose EDID the single head was given. The
    /// template's `Virtual` is only ever raised to it, per axis, never
    /// lowered: a later resize or a reattachment from a larger Deck must
    /// still fit the screen. `None` keeps the template's `Virtual`.
    pub served_size: Option<(u32, u32)>,
}

/// A rendered configuration and whether a requested depth could not be
/// applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedScreen {
    pub config: String,
    pub kept_template_depth: bool,
}

/// The `Virtual` a screen needs: the template's, raised per axis to the
/// served size. Never smaller than the template, which is the headroom later
/// resizes and reattachments rely on.
#[must_use]
pub fn raised_virtual(template: (u32, u32), served: (u32, u32)) -> (u32, u32) {
    (template.0.max(served.0), template.1.max(served.1))
}

fn parse_virtual(line: &str) -> Option<(u32, u32)> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "Virtual" {
        return None;
    }
    let width = fields.next()?.parse().ok()?;
    let height = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((width, height))
}

/// Rewrites the depth and `Virtual` lines of `config` for `screen`.
#[must_use]
pub fn with_session_screen(config: &str, screen: SessionScreen) -> RenderedScreen {
    let mut default_depth = Vec::new();
    let mut display_depth = Vec::new();
    let mut display_virtual = Vec::new();
    let mut in_display = false;
    let lines: Vec<&str> = config.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let keyword = trimmed.split_whitespace().next().unwrap_or_default();
        if keyword == "SubSection" && trimmed.contains("\"Display\"") {
            in_display = true;
        } else if keyword == "EndSubSection" {
            in_display = false;
        } else if keyword == "DefaultDepth" {
            default_depth.push(index);
        } else if in_display && keyword == "Depth" {
            display_depth.push(index);
        } else if in_display && keyword == "Virtual" {
            display_virtual.push(index);
        }
    }

    let mut rendered: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    let indent = |line: &str| line[..line.len() - line.trim_start().len()].to_owned();
    let mut kept_template_depth = false;
    if let Some(depth) = screen.depth {
        if let ([default_line], [display_line]) = (&default_depth[..], &display_depth[..]) {
            let value = depth.x11_depth();
            rendered[*default_line] =
                format!("{}DefaultDepth   {value}", indent(lines[*default_line]));
            rendered[*display_line] = format!("{}Depth      {value}", indent(lines[*display_line]));
        } else {
            kept_template_depth = true;
        }
    }
    if let (Some(served), [virtual_line]) = (screen.served_size, &display_virtual[..]) {
        // A `Virtual` line of another shape is the operator's; keep it.
        if let Some(template) = parse_virtual(lines[*virtual_line]) {
            let (width, height) = raised_virtual(template, served);
            if (width, height) != template {
                rendered[*virtual_line] = format!(
                    "{}Virtual    {width} {height}",
                    indent(lines[*virtual_line])
                );
            }
        }
    }

    let mut config_out = rendered.join("\n");
    if config.ends_with('\n') {
        config_out.push('\n');
    }
    RenderedScreen {
        config: config_out,
        kept_template_depth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packaged template's screen shape (the Linux host tests the
    /// packaged file itself).
    const PACKAGED: &str = r#"# Thirty bits. Both `DefaultDepth` and `Depth` have to say 30.
Section "Screen"
    Identifier     "Screen0"
    DefaultDepth   30
    Option         "AllowEmptyInitialConfiguration" "true"
    SubSection     "Display"
        Virtual    3840 2160
        Depth      30
    EndSubSection
EndSection
"#;

    fn changed_lines(before: &str, after: &str) -> Vec<(String, String)> {
        assert_eq!(before.lines().count(), after.lines().count());
        before
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect()
    }

    #[test]
    fn the_packaged_template_is_lowered_to_depth_24_on_exactly_two_lines() {
        let rendered = with_session_screen(
            PACKAGED,
            SessionScreen {
                depth: Some(DesktopDepth::Eight),
                served_size: None,
            },
        );
        assert!(!rendered.kept_template_depth);
        let changed = changed_lines(PACKAGED, &rendered.config);
        assert_eq!(
            changed,
            vec![
                (
                    "    DefaultDepth   30".to_owned(),
                    "    DefaultDepth   24".to_owned()
                ),
                (
                    "        Depth      30".to_owned(),
                    "        Depth      24".to_owned()
                ),
            ]
        );
        assert!(rendered.config.ends_with('\n'));
    }

    #[test]
    fn depth_30_leaves_the_packaged_template_unchanged() {
        let rendered = with_session_screen(
            PACKAGED,
            SessionScreen {
                depth: Some(DesktopDepth::Ten),
                served_size: None,
            },
        );
        assert!(!rendered.kept_template_depth);
        assert_eq!(rendered.config, PACKAGED);
        assert_eq!(
            with_session_screen(PACKAGED, SessionScreen::default()).config,
            PACKAGED
        );
    }

    fn virtual_after(served: (u32, u32)) -> Vec<(String, String)> {
        let rendered = with_session_screen(
            PACKAGED,
            SessionScreen {
                depth: None,
                served_size: Some(served),
            },
        );
        changed_lines(PACKAGED, &rendered.config)
    }

    #[test]
    fn a_served_size_within_the_template_keeps_its_virtual_headroom() {
        for served in [(2560, 1440), (1920, 1080), (3840, 2160), (1, 1), (0, 0)] {
            assert!(virtual_after(served).is_empty(), "{served:?}");
        }
    }

    #[test]
    fn a_larger_served_size_raises_the_virtual_screen_per_axis() {
        assert_eq!(
            virtual_after((5120, 2880)),
            vec![(
                "        Virtual    3840 2160".to_owned(),
                "        Virtual    5120 2880".to_owned()
            )]
        );
        // An ultra-wide display raises only the axis that needs it.
        assert_eq!(
            virtual_after((5120, 1440)),
            vec![(
                "        Virtual    3840 2160".to_owned(),
                "        Virtual    5120 2160".to_owned()
            )]
        );
        assert_eq!(
            virtual_after((2160, 3840)),
            vec![(
                "        Virtual    3840 2160".to_owned(),
                "        Virtual    3840 3840".to_owned()
            )]
        );
        assert_eq!(raised_virtual((3840, 2160), (2560, 1440)), (3840, 2160));
    }

    #[test]
    fn an_operator_shaped_virtual_line_is_kept() {
        for line in [
            "        Virtual    3840",
            "        Virtual    3840 2160 # x",
        ] {
            let template = PACKAGED.replace("        Virtual    3840 2160", line);
            let rendered = with_session_screen(
                &template,
                SessionScreen {
                    depth: None,
                    served_size: Some((7680, 4320)),
                },
            );
            assert_eq!(rendered.config, template, "{line}");
        }
    }

    #[test]
    fn an_operator_shaped_template_keeps_its_own_depth() {
        let no_display_depth = PACKAGED.replace("        Depth      30\n", "");
        let rendered = with_session_screen(
            &no_display_depth,
            SessionScreen {
                depth: Some(DesktopDepth::Eight),
                served_size: None,
            },
        );
        assert!(rendered.kept_template_depth);
        assert_eq!(rendered.config, no_display_depth);

        let two_screens = format!(
            "{PACKAGED}Section \"Screen\"\n    DefaultDepth   24\n    SubSection \"Display\"\n        Depth 24\n    EndSubSection\nEndSection\n"
        );
        let rendered = with_session_screen(
            &two_screens,
            SessionScreen {
                depth: Some(DesktopDepth::Eight),
                served_size: None,
            },
        );
        assert!(rendered.kept_template_depth);
        assert_eq!(rendered.config, two_screens);
    }

    #[test]
    fn comments_naming_the_keywords_are_never_rewritten() {
        assert!(PACKAGED.contains("`DefaultDepth`"));
        let rendered = with_session_screen(
            PACKAGED,
            SessionScreen {
                depth: Some(DesktopDepth::Eight),
                served_size: Some((1920, 1080)),
            },
        );
        for (before, after) in PACKAGED.lines().zip(rendered.config.lines()) {
            if before.trim_start().starts_with('#') {
                assert_eq!(before, after);
            }
        }
    }

    #[test]
    fn a_depth_line_outside_the_display_subsection_is_not_the_display_depth() {
        let config = "Section \"Screen\"\n    DefaultDepth 30\n    Depth 30\n    SubSection \"Display\"\n        Depth 30\n        Virtual 3840 2160\n    EndSubSection\nEndSection";
        let rendered = with_session_screen(
            config,
            SessionScreen {
                depth: Some(DesktopDepth::Eight),
                served_size: Some((5120, 1200)),
            },
        );
        assert!(!rendered.kept_template_depth);
        assert_eq!(
            rendered.config,
            "Section \"Screen\"\n    DefaultDepth   24\n    Depth 30\n    SubSection \"Display\"\n        Depth      24\n        Virtual    5120 2160\n    EndSubSection\nEndSection"
        );
    }
}
