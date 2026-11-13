use super::*;
use crate::mermaid::{
    contrast,
    tests::{light, terminal_dark},
};
use std::fmt::Write as _;

#[test]
fn sankey_labels_remain_readable_over_gradients_and_overlapping_ribbons() {
    for theme in [terminal_dark(), light()] {
        let p = Palette::new(&theme);
        let (_, _, svg) = rendered(
            "sankey-beta\nA,B,10\nA,C,15\nB,D,8\nC,E,12\nD,F,5\nE,G,9\nF,H,4\nG,I,8\nH,J,4",
            &p,
        );
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let label_ink = doc
            .descendants()
            .filter(|n| n.has_tag_name("text"))
            .map(|n| rgb(n.attribute("fill").unwrap()))
            .collect::<Vec<_>>();
        assert_ne!(label_ink, Vec::<[u8; 4]>::new());
        let mut underlays = Vec::new();
        for ribbon in doc
            .descendants()
            .filter(|n| n.has_tag_name("path") && n.has_attribute("stroke-opacity"))
        {
            let opacity: f32 = ribbon.attribute("stroke-opacity").unwrap().parse().unwrap();
            let id = ribbon
                .attribute("stroke")
                .unwrap()
                .strip_prefix("url(#")
                .unwrap()
                .strip_suffix(')')
                .unwrap();
            let gradient = doc
                .descendants()
                .find(|n| n.attribute("id") == Some(id))
                .unwrap();
            let stops = gradient
                .children()
                .filter(|n| n.has_tag_name("stop"))
                .map(|n| rgb(n.attribute("stop-color").unwrap()))
                .collect::<Vec<_>>();
            assert_eq!(stops.len(), 2);
            // Sample the actual rendered gradient, including endpoints and
            // midpoint, rather than checking a fill against the empty canvas.
            for step in 0_u8..=20 {
                let color = mix(stops[0], stops[1], f32::from(step) / 20.0);
                let underlay = mix(p.bg, color, opacity);
                for &ink in &label_ink {
                    assert!(contrast(ink, underlay) >= 4.5);
                }
                underlays.push((color, opacity));
            }
        }
        assert!(underlays.len() >= 9 * 21);
        // Crossings compound opacity. Every pair of actual sampled ribbons
        // must still support their foreground labels when composited in order.
        for &(first, first_alpha) in &underlays {
            for &(second, second_alpha) in &underlays {
                let crossing = mix(mix(p.bg, first, first_alpha), second, second_alpha);
                assert!(contrast(p.fg, crossing) >= 4.5);
            }
        }
        for node in doc.descendants().filter(|n| n.has_tag_name("rect")) {
            let fill = rgb(node.attribute("fill").unwrap());
            let outline = rgb(node.attribute("stroke").unwrap());
            assert!(contrast(outline, p.bg) >= 3.0);
            assert!(contrast(outline, fill) >= 3.0);
        }
    }
}

#[test]
fn architecture_tiles_and_glyphs_remain_visible() {
    for theme in [terminal_dark(), light()] {
        let p = Palette::new(&theme);
        let (_, _, svg) = rendered("architecture-beta\nservice api(server)[API]", &p);
        let doc = roxmltree::Document::parse(&svg).unwrap();
        assert!(!svg.contains("#4a72d6"));
        let tile = doc
            .descendants()
            .find(|n| n.has_tag_name("rect") && n.attribute("rx") == Some("6"))
            .unwrap();
        let surface = rgb(tile.attribute("fill").unwrap());
        assert!(contrast(rgb(tile.attribute("stroke").unwrap()), surface) >= 3.0);
        let glyphs = doc
            .descendants()
            .filter(|n| {
                !n.has_tag_name("text")
                    && ["fill", "stroke"]
                        .iter()
                        .any(|paint| n.attribute(*paint) == Some(hex(p.fg).as_str()))
            })
            .count();
        assert!(glyphs > 0);
        assert!(contrast(p.fg, surface) >= 4.5);
    }
}

#[test]
fn eight_git_lanes_keep_distinct_readable_pills_and_visible_commit_ink() {
    let mut source = "gitGraph\ncommit\n".to_owned();
    for i in 1..8 {
        writeln!(source, "branch lane{i}\ncheckout lane{i}\ncommit").unwrap();
    }
    for theme in [terminal_dark(), light()] {
        let p = Palette::new(&theme);
        let (_, _, svg) = rendered(&source, &p);
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let mut pills = std::collections::BTreeSet::new();
        let mut commits = std::collections::BTreeSet::new();
        for node in doc.descendants() {
            if node.has_tag_name("rect")
                && node.attribute("rx") == Some("10")
                && node.attribute("ry") == Some("10")
            {
                let fill = node.attribute("fill").unwrap();
                let label = node.next_sibling_element().unwrap();
                assert!(contrast(rgb(label.attribute("fill").unwrap()), rgb(fill)) >= 4.5);
                pills.insert(fill);
            }
            if node.has_tag_name("circle") {
                let fill = node.attribute("fill").unwrap();
                assert!(contrast(rgb(fill), p.bg) >= 3.0);
                commits.insert(fill);
            }
        }
        assert_eq!(pills.len(), 8, "branch identities must not collapse");
        assert_eq!(commits.len(), 8, "lane/commit identities must not collapse");
    }
}

fn rendered(source: &str, palette: &Palette) -> (Diagram, String, String) {
    let diagram = mermaid_svg::parse(source).expect("valid fixture");
    let before = mermaid_svg::render_diagram_with(&diagram, &palette.mermaid_theme()).unwrap();
    let after = apply(&before, &diagram, palette).unwrap();
    (diagram, before, after)
}

#[test]
fn all_gallery_types_preserve_geometry_text_fonts_and_semantics() {
    let gallery = include_str!("../../../../docs/mermaid-gallery.md");
    let mut checked = 0;
    for theme in [terminal_dark(), light()] {
        let palette = Palette::new(&theme);
        for block in gallery.split("```mermaid\n").skip(1) {
            let source = block.split("\n```").next().unwrap();
            let (diagram, before, after) = rendered(source, &palette);
            let a = roxmltree::Document::parse(&before).unwrap();
            let b = roxmltree::Document::parse(&after).unwrap();
            assert_eq!(a.descendants().count(), b.descendants().count());
            for (old, new) in a.descendants().zip(b.descendants()) {
                assert_eq!(old.tag_name(), new.tag_name());
                if old.is_text() {
                    assert_eq!(&before[old.range()], &after[new.range()], "{source}");
                }
                for attribute in old.attributes() {
                    let name = attribute.name();
                    // Only the explicitly approved presentation properties
                    // can differ. Coordinates, paths, fonts, marker IDs,
                    // dash patterns, links, dimensions and text are exact.
                    if matches!(
                        name,
                        "fill"
                            | "stroke"
                            | "stop-color"
                            | "fill-opacity"
                            | "stroke-width"
                            | "stroke-opacity"
                            | "filter"
                    ) {
                        continue;
                    }
                    if name == "rx" && matches!(diagram, Diagram::Sequence(_) | Diagram::Class(_)) {
                        continue;
                    }
                    assert_eq!(
                        Some(attribute.value()),
                        new.attribute(name),
                        "{} changed {name}",
                        source.lines().next().unwrap()
                    );
                }
                if old.ancestors().any(|n| n.has_tag_name("marker"))
                    || old.has_attribute("stroke-dasharray")
                    || old.attribute("stroke-width") == Some("3.5")
                {
                    assert_eq!(old.attribute("stroke-width"), new.attribute("stroke-width"));
                    assert_eq!(
                        old.attribute("stroke-linejoin"),
                        new.attribute("stroke-linejoin")
                    );
                }
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 46);
}

#[test]
fn author_styles_matching_defaults_opt_out_without_losing_attributes() {
    let palette = Palette::new(&terminal_dark());
    for source in [
        "flowchart LR\n A[White] --> B[Black]\nstyle A fill:#fff,stroke:#333,stroke-width:1.5px\nlinkStyle 0 stroke:#333,stroke-width:1.5px",
        "classDiagram\n class A\nstyle A fill:#fff,stroke:#333",
        "stateDiagram-v2\n A --> B\nclassDef chosen fill:#fff,stroke:#333\nclass A chosen",
        "erDiagram\n A ||--o{ B : owns\nstyle A fill:#fff,stroke:#333",
        "sequenceDiagram\nrect white\nA->>B: custom\nend",
        "block-beta\n A[Card]\nstyle A fill:#fff,stroke:#333",
        "C4Context\n Person(a,\"Author\")\nUpdateElementStyle(a, $bgColor=\"#08427B\", $fontColor=\"#FFFFFF\")",
        "---\nconfig:\n  sankey:\n    linkColor: '#4e79a7'\n---\nsankey-beta\nA,B,10",
    ] {
        let (diagram, before, after) = rendered(source, &palette);
        assert!(
            has_custom_styles(&diagram),
            "fixture must contain explicit style: {source}"
        );
        assert_eq!(before, after, "author style was changed: {source}");
    }
}

#[test]
fn xml_splicing_preserves_quotes_escaped_labels_and_custom_attribute_names() {
    let palette = Palette::new(&terminal_dark());
    let diagram = mermaid_svg::parse("flowchart TD\nA-->B").unwrap();
    let svg = r##"<svg><rect data-fill="#fff" data-stroke="black" data-tip="a > b" fill='#fff' stroke="black"/><text id="white" fill="#ffffff">&lt;rect fill="#fff"&gt; &amp; #333<tspan fill="#fff">custom rich span</tspan></text><a href="https://example.com/?fill=%23fff&amp;x=1"><rect fill="#123456"/></a></svg>"##;
    let out = apply(svg, &diagram, &palette).unwrap();
    let expected = format!(
        r##"<svg><rect data-fill="#fff" data-stroke="black" data-tip="a > b" fill='{}' stroke="{}"/><text id="white" fill="{}">&lt;rect fill="#fff"&gt; &amp; #333<tspan fill="#fff">custom rich span</tspan></text><a href="https://example.com/?fill=%23fff&amp;x=1"><rect fill="#123456"/></a></svg>"##,
        hex(palette.bg),
        hex(palette.fg),
        hex(palette.fg)
    );
    assert_eq!(out, expected);
    assert!(apply("<svg><rect>", &diagram, &palette).is_err());
}

#[test]
fn only_default_participant_and_class_cards_get_rounded() {
    let palette = Palette::new(&terminal_dark());
    for source in [
        "sequenceDiagram\nparticipant A\nparticipant B\nA->>+B: call\nB-->>-A: reply",
        "classDiagram\nclass A {\n +String name\n +run()\n}\nA <|-- B",
    ] {
        let (_, before, after) = rendered(source, &palette);
        let old = roxmltree::Document::parse(&before).unwrap();
        let new = roxmltree::Document::parse(&after).unwrap();
        let mut cards = 0;
        for (a, b) in old.descendants().zip(new.descendants()) {
            if a.has_tag_name("rect")
                && matches!(a.attribute("rx"), Some("2" | "4"))
                && a.attribute("fill") == Some(hex(palette.fill).as_str())
            {
                assert_eq!(b.attribute("rx"), Some("6"));
                assert_eq!(b.attribute("stroke-width"), Some("1.25"));
                cards += 1;
            } else {
                assert_eq!(a.attribute("rx"), b.attribute("rx"));
            }
        }
        assert!(cards >= 2);
    }
    for source in [
        "flowchart LR\nA[Rect] --> B(Round) --> C([Stadium]) --> D[(Cylinder)] --> E{Choice}\n E ==> F[[Subroutine]]",
        "erDiagram\nA {\n int id PK\n string name\n}\nA ||--o{ B : owns",
        "packet-beta\n0-15: \"Header\"\n16-31: \"Data\"",
    ] {
        let (_, before, after) = rendered(source, &palette);
        let a = roxmltree::Document::parse(&before).unwrap();
        let b = roxmltree::Document::parse(&after).unwrap();
        for (a, b) in a.descendants().zip(b.descendants()) {
            assert_eq!(a.attribute("rx"), b.attribute("rx"));
            assert_eq!(a.attribute("ry"), b.attribute("ry"));
        }
    }
}

fn rgb(s: &str) -> Rgba {
    let n = u32::from_str_radix(s.strip_prefix('#').expect(s), 16).unwrap();
    [(n >> 16) as u8, (n >> 8) as u8, n as u8, 255]
}

#[test]
fn gantt_milestone_and_status_labels_are_readable_in_both_themes() {
    for theme in [terminal_dark(), light()] {
        let palette = Palette::new(&theme);
        let (_, before, after) = rendered(
            "gantt\n dateFormat YYYY-MM-DD\n section Build\n Normal :2026-01-01, 3d\n Active :active, 2026-01-01, 3d\n Done :done, 2026-01-01, 3d\n Critical :crit, 2026-01-01, 3d\n Freeze :vert, 2026-01-02, 0d",
            &palette,
        );
        assert!(before.contains("#000080"));
        assert!(!after.contains("#000080"));
        let doc = roxmltree::Document::parse(&after).unwrap();
        let marker = doc
            .descendants()
            .find(|n| n.has_tag_name("text") && n.text() == Some("Freeze"))
            .unwrap();
        assert!(contrast(rgb(marker.attribute("fill").unwrap()), palette.bg) >= 4.5);
        for node in doc.descendants().filter(|n| n.has_tag_name("rect")) {
            if node.has_attribute("stroke") && !node.has_attribute("fill-opacity") {
                let fill = rgb(node.attribute("fill").unwrap());
                assert!(contrast(palette.fg, fill) >= 4.5);
                assert!(contrast(rgb(node.attribute("stroke").unwrap()), fill) >= 3.0);
            }
        }
    }
}

#[test]
fn badge_pill_icon_and_tag_roles_keep_contrasting_ink() {
    for theme in [terminal_dark(), light()] {
        let p = Palette::new(&theme);
        let (_, _, svg) = rendered("sequenceDiagram\nautonumber\nA->>B: Hello", &p);
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let number = doc
            .descendants()
            .find(|n| n.has_tag_name("text") && n.text() == Some("1"))
            .unwrap();
        let badge = number.prev_sibling_element().unwrap();
        assert_eq!(badge.attribute("r"), Some("9"));
        assert!(
            contrast(
                rgb(number.attribute("fill").unwrap()),
                rgb(badge.attribute("fill").unwrap())
            ) >= 4.5
        );

        let (_, _, svg) = rendered(
            "gitGraph\ncommit tag:\"v1\"\nbranch green\ncheckout green\ncommit\nbranch yellow\ncheckout yellow\ncommit",
            &p,
        );
        let doc = roxmltree::Document::parse(&svg).unwrap();
        for label in ["main", "green", "yellow", "v1"] {
            let text = doc
                .descendants()
                .find(|n| n.has_tag_name("text") && n.text() == Some(label))
                .unwrap();
            // Tags have a punch-hole circle between body and label.
            let mut body = text.prev_sibling_element().unwrap();
            if body.has_tag_name("circle") {
                body = body.prev_sibling_element().unwrap();
            }
            assert!(
                contrast(
                    rgb(text.attribute("fill").unwrap()),
                    rgb(body.attribute("fill").unwrap())
                ) >= 4.5,
                "git label {label} on {}",
                body.attribute("fill").unwrap()
            );
        }
        assert!(!svg.contains("#fff5ad"));

        let (_, _, svg) = rendered(
            "C4Context\nPerson(user,\"User\")\nSystem(system,\"System\")\nSystem_Ext(ext,\"External\")\nRel(user,system,\"uses\")",
            &p,
        );
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let icon = doc
            .descendants()
            .find(|n| n.has_tag_name("g") && n.attribute("stroke") == Some("none"))
            .unwrap();
        let person_fill = readable_tint(p.bg, p.accents[0], p.fg, 0.16);
        assert!(contrast(rgb(icon.attribute("fill").unwrap()), person_fill) >= 3.0);
        for card in doc.descendants().filter(|n| {
            n.has_tag_name("rect") && n.attribute("stroke").is_some_and(|s| s != "none")
        }) {
            let fill = rgb(card.attribute("fill").unwrap());
            assert!(contrast(p.fg, fill) >= 4.5);
            assert!(contrast(rgb(card.attribute("stroke").unwrap()), fill) >= 3.0);
        }

        let (_, _, svg) = rendered("mindmap\n root((Root))\n  Child", &p);
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let label = doc
            .descendants()
            .find(|n| n.has_tag_name("text") && n.text() == Some("Root"))
            .unwrap();
        let root = label.prev_sibling_element().unwrap();
        assert!(
            contrast(
                rgb(label.attribute("fill").unwrap()),
                rgb(root.attribute("fill").unwrap())
            ) >= 4.5
        );
        assert_eq!(root.attribute("filter"), Some("none"));

        // The today marker otherwise depends on the machine's clock.
        let diagram = mermaid_svg::parse("gantt\nA :2026-01-01, 1d").unwrap();
        let svg = apply(
            r##"<svg><line stroke="#d33"/><text fill="#d33">Today</text></svg>"##,
            &diagram,
            &p,
        )
        .unwrap();
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let text = doc.descendants().find(|n| n.has_tag_name("text")).unwrap();
        assert!(contrast(rgb(text.attribute("fill").unwrap()), p.bg) >= 4.5);
    }
}
