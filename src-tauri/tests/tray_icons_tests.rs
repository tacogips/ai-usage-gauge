use ccusage_core::types::{MenuBarStatusEntry, PieChartId};
use ccusage_gauge_app::tray_icons::{TrayIconRenderer, TrayPresentationDedup, TrayRenderState};
fn entry(id: PieChartId, fraction: Option<f64>) -> MenuBarStatusEntry {
    MenuBarStatusEntry {
        id,
        fraction,
        show_icon: true,
        ratio_label: None,
        is_budget_unset: false,
        is_budget_unknown: false,
    }
}
fn state(entries: Vec<MenuBarStatusEntry>) -> TrayRenderState {
    TrayRenderState {
        entries,
        is_usage_unavailable: false,
        has_warnings: false,
    }
}
#[test]
fn assets_and_masks() {
    for (bytes, size) in [
        (
            include_bytes!("../assets/claude-template.png").as_slice(),
            72,
        ),
        (
            include_bytes!("../assets/chatgpt-template.png").as_slice(),
            36,
        ),
    ] {
        let reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        assert_eq!((reader.info().width, reader.info().height), (size, size));
    }
    let r = TrayIconRenderer::new();
    for a in [&r.claude, &r.codex, &r.gemini] {
        assert!(a.solid.iter().any(|v| *v > 0));
        assert!(a.interior.iter().any(|v| *v > 0));
        assert!(a.outline.iter().any(|v| *v > 0));
        assert!(a
            .cumulative_interior_by_row
            .windows(2)
            .all(|w| w[0] <= w[1]));
    }
}
#[test]
fn area_fill_and_unavailable() {
    let r = TrayIconRenderer::new();
    assert!(r.gemini.fill_height_fraction(0.05) > 0.15);
    assert_eq!(r.gemini.fill_height_fraction(0.0), 0.0);
    assert_eq!(r.gemini.fill_height_fraction(1.0), 1.0);
    let draw = |f| {
        r.render_strip(&state(vec![entry(PieChartId::Gemini5h, f)]))
            .unwrap()
    };
    let mut prev = 0;
    for f in [0.0, 0.05, 0.5, 1.0] {
        let img = draw(Some(f));
        assert!(img
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[..3] == [0, 0, 0]));
        let sum: usize = img
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3] as usize)
            .sum();
        assert!(sum >= prev);
        prev = sum;
    }
    assert_ne!(draw(None), draw(Some(0.0)));
}
#[test]
fn budget_variants_and_warning() {
    let r = TrayIconRenderer::new();
    let mut images = Vec::new();
    for f in [0.0, 0.5, 1.0] {
        images.push(
            r.render_strip(&state(vec![entry(PieChartId::Budget, Some(f))]))
                .unwrap(),
        );
    }
    let mut e = entry(PieChartId::Budget, None);
    e.is_budget_unknown = true;
    images.push(r.render_strip(&state(vec![e.clone()])).unwrap());
    e.is_budget_unknown = false;
    e.is_budget_unset = true;
    images.push(r.render_strip(&state(vec![e])).unwrap());
    let mut s = state(vec![entry(PieChartId::Budget, None)]);
    s.has_warnings = true;
    images.push(r.render_strip(&s).unwrap());
    for (i, a) in images.iter().enumerate() {
        assert_eq!((a.width, a.height), (36, 36));
        assert!(a.rgba.iter().any(|v| *v > 0));
        for b in &images[i + 1..] {
            assert_ne!(a, b);
        }
    }
    s.has_warnings = false;
    s.is_usage_unavailable = true;
    assert_eq!(r.render_strip(&s).unwrap(), *images.last().unwrap());
}
#[test]
fn layout_and_dedup() {
    let r = TrayIconRenderer::new();
    assert!(r.render_strip(&state(vec![])).is_none());
    let mut a = entry(PieChartId::Claude5h, Some(0.45));
    a.ratio_label = Some("45%".into());
    let mut b = entry(PieChartId::Budget, None);
    b.show_icon = false;
    b.ratio_label = Some("--".into());
    let s = state(vec![a, b]);
    let image = r.render_strip(&s).unwrap();
    assert_eq!((image.width, image.height), (6 + 24 + 14 + 36 + 6 + 36, 36));
    for range in [6..30, 86..122] {
        assert!((0..36).any(|y| range
            .clone()
            .any(|x| image.rgba[(y * image.width as usize + x) * 4 + 3] > 0)));
    }
    assert!(image.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 192));
    let mut d = TrayPresentationDedup::default();
    assert!(d.update_icon(&s));
    assert!(!d.update_icon(&s));
    let mut t = s.clone();
    t.has_warnings = true;
    assert!(d.update_icon(&t));
    assert!(d.update_title("$1"));
    assert!(!d.update_title("$1"));
    assert!(d.update_title("$2"));
    assert!(d.update_tooltip("ok"));
    assert!(!d.update_tooltip("ok"));
    assert!(d.update_tooltip("new"));
}

#[test]
fn fractional_budget_label_draws_decimal_point() {
    let mut budget = entry(PieChartId::Budget, None);
    budget.show_icon = false;
    budget.ratio_label = Some("12.5%".into());
    let image = TrayIconRenderer::new()
        .render_strip(&state(vec![budget]))
        .unwrap();
    assert_eq!((image.width, image.height), (66, 36));
    assert!(image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[..3] == [0, 0, 0]));
    assert!((0..36).any(|y| (30..42).any(|x| image.rgba[(y * 66 + x) * 4 + 3] > 0)));
}
