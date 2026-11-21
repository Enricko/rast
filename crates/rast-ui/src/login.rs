//! The classic RAST lock screen: Alpine Dawn, a brand panel and device login.

use rune::{
    Color, Cx, El, Image, ImageCache, ScrollSpec, SurfaceFit, TextStyle, Waker, col, el, row, text,
    text_input,
};

use crate::{Screen, State};

fn rgb(hex: u32) -> Color {
    use rune::color::srgb_decode;
    Color {
        r: srgb_decode(((hex >> 16) & 255) as f32 / 255.0),
        g: srgb_decode(((hex >> 8) & 255) as f32 / 255.0),
        b: srgb_decode((hex & 255) as f32 / 255.0),
        a: 1.0,
    }
}

const SKY: u32 = 0x354659;
const BLUE: u32 = 0x2276b6;
const FOCUS: u32 = 0x57b3ee;
const PALE: u32 = 0xd1dae3;
const SMALL: TextStyle = TextStyle::new(12.0, 470.0);
const BODY: TextStyle = TextStyle::new(13.0, 440.0);
const LABEL: TextStyle = TextStyle::new(12.0, 560.0);

macro_rules! svg {
    ($($part:literal),+ $(,)?) => {
        concat!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="1.65" stroke-linecap="round" stroke-linejoin="round">"#, $($part,)+ "</svg>").as_bytes()
    };
}

pub(crate) struct Art {
    wallpaper: Image,
    logo: Image,
    files: Image,
    people: Image,
    activity: Image,
    sun: Image,
    moon: Image,
}

impl Art {
    pub(crate) fn load(wake: Waker) -> Self {
        let cache = ImageCache::new(wake);
        let art = Self {
            wallpaper: cache.load(&include_bytes!("../assets/alpine-dawn.jpg")[..]),
            logo: cache.svg(svg!(r#"<path d="M12 2.6 20.4 7v10L12 21.4 3.6 17V7z"/><path fill="white" stroke="none" d="M12 7.2 15.8 9.4v4.4L12 16l-3.8-2.2V9.4z"/>"#), 30.0, 3.0),
            files: cache.svg(svg!(r#"<path d="M3.5 6.2h5.1l1.8 2.3h10.1v9.9a1.6 1.6 0 0 1-1.6 1.6H3.5zm0 6.4h17"/>"#), 18.0, 3.0),
            people: cache.svg(svg!(r#"<circle cx="9.2" cy="8.6" r="3.2"/><path d="M3.4 19.4c.7-3.2 3-4.9 5.8-4.9s5.1 1.7 5.8 4.9M15.4 5.9a3 3 0 0 1 0 5.6M17 14.9c2 .5 3.3 2 3.6 4.2"/>"#), 18.0, 3.0),
            activity: cache.svg(svg!(r#"<path d="M3.4 12.4h4l2.6-6.8 4.1 13 2.5-6.2h4"/>"#), 18.0, 3.0),
            sun: cache.svg(svg!(r#"<circle cx="12" cy="12" r="4.2"/><path d="M12 2.8v2.4m0 13.6v2.4M2.8 12h2.4m13.6 0h2.4M5.5 5.5l1.7 1.7m9.6 9.6 1.7 1.7m0-13-1.7 1.7m-9.6 9.6-1.7 1.7"/>"#), 17.0, 3.0),
            moon: cache.svg(svg!(r#"<path d="M20.2 14.6A8.4 8.4 0 0 1 9.4 3.8a8.4 8.4 0 1 0 10.8 10.8z"/>"#), 17.0, 3.0),
        };
        #[cfg(not(target_arch = "wasm32"))]
        if rune::is_headless() {
            for image in [
                &art.wallpaper,
                &art.logo,
                &art.files,
                &art.people,
                &art.activity,
                &art.sun,
                &art.moon,
            ] {
                let _ = image.wait();
            }
        }
        art
    }
}

struct Palette {
    card: Color,
    ink: Color,
    muted: Color,
    field: Color,
    line: Color,
    error: Color,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                card: rgb(0x1e252f),
                ink: rgb(0xf0f5fb),
                muted: rgb(0xb3bfce),
                field: rgb(0x1a212b),
                line: rgb(0x495665),
                error: rgb(0xffa7ab),
            }
        } else {
            Self {
                card: rgb(0xf8fafc),
                ink: rgb(0x202e3d),
                muted: rgb(0x4e6174),
                field: Color::WHITE,
                line: rgb(0xaabaca),
                error: rgb(0x9d2234),
            }
        }
    }
}

pub(crate) fn view(state: &State, cx: &mut Cx<State>) -> El {
    cx.web_focus();
    cx.focus_ring(2.0, 2.0, rgb(FOCUS));
    let [width, height] = cx.layout_size("lock-screen").unwrap_or([1280.0, 800.0]);
    let wide = width >= 860.0 && height >= 560.0;
    let short = height < 630.0;
    let creating = state.screen == Screen::CreateAccount;
    let dark = state.login_dark;
    let p = Palette::new(dark);
    let art = state.login_art.as_ref();

    let mut backdrop = el()
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(rgb(SKY));
    if let Some(art) = art {
        backdrop = backdrop.pixels_fit(art.wallpaper.pixels(), SurfaceFit::Fill);
    }
    let tint = el()
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .gradient(rgb(0x131b27).alpha(0.39), rgb(0x0e1c28).alpha(0.59));
    let theme_label = if dark {
        "Switch to light mode"
    } else {
        "Switch to dark mode"
    };
    let theme_icon = art.map(|a| if dark { &a.sun } else { &a.moon });
    let toggle = el()
        .key("login-theme")
        .role("button")
        .label(theme_label)
        .size(38.0, 38.0)
        .radius(10.0)
        .center()
        .fill(rgb(0x17222e).alpha(0.94))
        .border(1.0, Color::WHITE.alpha(0.20))
        .hover(rgb(0x2b3b4b))
        .pin(None, Some(14.0), Some(16.0), None)
        .child(symbol(theme_icon, if dark { "☀" } else { "☾" }, 17.0))
        .on_press(|s: &mut State| s.login_dark = !s.login_dark);

    let card = auth_card(state, &p, creating, wide, short);
    let layout = if wide {
        let right = col()
            .w((width * 0.46).min(480.0))
            .h_full()
            .px(28.0)
            .center()
            .child(card)
            .child(
                row()
                    .key("auth-footer")
                    .pin(Some(28.0), None, Some(28.0), Some(32.0))
                    .justify_between()
                    .child(text("RAST · Revamp").font(SMALL).color(rgb(PALE)))
                    .child(
                        text("Local unlock · No server session")
                            .font(SMALL)
                            .color(rgb(PALE)),
                    ),
            );
        row()
            .w_full()
            .max_w(1072.0)
            .h_full()
            .child(brand_panel(art))
            .child(right)
    } else {
        let content = col()
            .w_full()
            .max_w(480.0)
            .min_h(height.max(0.0))
            .center()
            .px(16.0)
            .pt(55.0)
            .pb(24.0)
            .gap(16.0)
            .child(brand_lockup(art, true))
            .child(card);
        col()
            .w_full()
            .h_full()
            .center_y()
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(content)
    };

    col()
        .key("lock-screen")
        .w_full()
        .h_full()
        .center_y()
        .fill(rgb(SKY))
        .child(backdrop)
        .child(tint)
        .child(layout)
        .child(toggle)
}

fn symbol(image: Option<&Image>, fallback: &'static str, width: f32) -> El {
    if let Some(image) = image {
        el().size(width, width)
            .pixels_fit(image.pixels(), SurfaceFit::Fit)
    } else {
        text(fallback)
            .font(TextStyle::new(width, 570.0))
            .color(Color::WHITE)
            .inert()
    }
}

fn brand_lockup(art: Option<&Art>, compact: bool) -> El {
    let size = if compact { 42.0 } else { 56.0 };
    row()
        .key("auth-brand")
        .gap(14.0)
        .center_y()
        .child(
            el().size(size, size)
                .radius(if compact { 13.0 } else { 17.0 })
                .center()
                .gradient(rgb(0x45a7eb), rgb(0x3089cf))
                .border(1.0, Color::WHITE.alpha(0.24))
                .child(symbol(
                    art.map(|a| &a.logo),
                    "⬡",
                    if compact { 23.0 } else { 30.0 },
                )),
        )
        .child(
            col()
                .gap(3.0)
                .shrink()
                .child(
                    text("R A S T")
                        .font(TextStyle::new(if compact { 17.0 } else { 24.0 }, 780.0))
                        .color(Color::WHITE),
                )
                .child(
                    text("Resource Administration & Storage Toolkit")
                        .wrap()
                        .font(SMALL)
                        .color(rgb(PALE)),
                ),
        )
}

fn brand_panel(art: Option<&Art>) -> El {
    col().key("auth-brand-panel").grow(1.0).min_w(0.0).h_full().px(54.0).center()
        .gradient(rgb(0x111722).alpha(0.65), rgb(0x101923).alpha(0.82))
        .child(col().w_full().max_w(430.0).gap(24.0)
            .child(brand_lockup(art, false))
            .child(text("A calm, fast home for your self‑hosted server. Everything stays on your hardware.")
                .w_full().wrap().font(TextStyle::new(28.0, 690.0)).color(rgb(0xe1e7ee)))
            .child(col().w_full().gap(14.0)
                .child(feature(art.map(|a| &a.files), "Files & shared folders", "Browse, upload and manage server storage."))
                .child(feature(art.map(|a| &a.people), "People & permissions", "Accounts, quotas, roles and an audit trail."))
                .child(feature(art.map(|a| &a.activity), "Live operations", "Monitoring, containers and backups in one place.")))
            .child(text("Self-hosted · No telemetry · Revamp preview").w_full().wrap().mt(6.0).font(SMALL).color(rgb(PALE))))
}

fn feature(image: Option<&Image>, title: &'static str, detail: &'static str) -> El {
    row()
        .w_full()
        .gap(12.0)
        .center_y()
        .child(
            el().size(34.0, 34.0)
                .radius(10.0)
                .center()
                .fill(Color::WHITE.alpha(0.11))
                .border(1.0, Color::WHITE.alpha(0.17))
                .child(symbol(image, "·", 18.0)),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(3.0)
                .child(
                    text(title)
                        .w_full()
                        .wrap()
                        .font(TextStyle::new(13.0, 650.0))
                        .color(Color::WHITE),
                )
                .child(text(detail).w_full().wrap().font(SMALL).color(rgb(PALE))),
        )
}

fn auth_card(state: &State, p: &Palette, creating: bool, wide: bool, short: bool) -> El {
    let platform = if cfg!(target_arch = "wasm32") {
        "browser"
    } else {
        "device"
    };
    let (title, subtitle) = if creating {
        (
            "Welcome to RAST",
            format!("Set up a local login for this {platform}."),
        )
    } else {
        ("Welcome back", "Sign in to your RAST workspace.".to_owned())
    };
    let pad = if wide && !short { 32.0 } else { 22.0 };
    let username = field(&state.username_field, p, "username", "Username", "username")
        .on_edit(|s: &mut State| &mut s.username_field)
        .autofocus();
    let password = field(
        &state.password_field,
        p,
        "password",
        "Password",
        if creating {
            "at least 8 characters"
        } else {
            "password"
        },
    )
    .pr(65.0)
    .secure()
    .revealed(state.reveal_password)
    .on_edit(|s: &mut State| &mut s.password_field)
    .on_submit(|s: &mut State, tx| {
        if s.screen == Screen::CreateAccount {
            tx.focus("confirm", true);
        } else {
            s.submit_login();
        }
    });
    let confirm = field(
        &state.confirm_field,
        p,
        "confirm",
        "Confirm password",
        "repeat the password",
    )
    .secure()
    .revealed(state.reveal_password)
    .on_edit(|s: &mut State| &mut s.confirm_field)
    .on_submit(|s: &mut State, _tx| s.submit_create());
    let show = if state.reveal_password {
        "Hide"
    } else {
        "Show"
    };
    let reveal = el()
        .key(if state.reveal_password {
            "hide-password"
        } else {
            "show-password"
        })
        .role("button")
        .label(format!("{show} password"))
        .size(56.0, 32.0)
        .center()
        .radius(5.0)
        .hover(p.line.alpha(0.35))
        .pin(None, Some(4.0), Some(4.0), None)
        .preserve_focus_on_press(true)
        .child(text(show).font(LABEL).color(p.muted))
        .on_press(State::toggle_password_visibility);
    let fields = col()
        .w_full()
        .gap(if short { 9.0 } else { 12.0 })
        .child(labeled(p, "Username", username))
        .child(labeled(
            p,
            "Password",
            col().w_full().child(password).child(reveal),
        ));
    let fields = if creating {
        fields.child(labeled(p, "Confirm password", confirm))
    } else {
        fields
    };

    let (submit_key, submit_label, submit_action): (_, _, fn(&mut State)) = if creating {
        ("create-account", "Create local login", State::submit_create)
    } else {
        ("sign-in", "Sign in", State::submit_login)
    };
    let submit = row()
        .key(submit_key)
        .role("button")
        .label(submit_label)
        .center()
        .gap(7.0)
        .w_full()
        .h(38.0)
        .radius(6.0)
        .fill(rgb(BLUE))
        .hover(rgb(0x226da7))
        .child(
            text("→")
                .font(TextStyle::new(15.0, 600.0))
                .color(Color::WHITE)
                .inert(),
        )
        .child(
            text(submit_label)
                .font(TextStyle::new(13.0, 650.0))
                .color(Color::WHITE),
        )
        .on_press(submit_action);
    let mut card = col()
        .key("auth-card")
        .w_full()
        .max_w(432.0)
        .padding(pad)
        .gap(if short { 11.0 } else { 16.0 })
        .radius(12.0)
        .fill(p.card)
        .border(1.0, p.line.alpha(0.62))
        .shadow(14.0, 34.0, rgb(0x07121c).alpha(0.48))
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(text(title).font(TextStyle::new(22.0, 720.0)).color(p.ink))
                .child(text(subtitle).w_full().wrap().font(BODY).color(p.muted)),
        )
        .child(fields);
    if let Some(error) = &state.login_error {
        card = card.child(
            text(error.clone())
                .key("login-error")
                .w_full()
                .wrap()
                .font(SMALL)
                .color(p.error),
        );
    }
    card.child(submit).child(
        text(format!(
            "This unlocks only this {platform}. It does not sign you in to a server."
        ))
        .w_full()
        .wrap()
        .font(SMALL)
        .color(p.muted),
    )
}

fn field(
    model: &rune::TextEdit,
    p: &Palette,
    key: &'static str,
    label: &'static str,
    placeholder: &'static str,
) -> El {
    text_input(model, BODY)
        .key(key)
        .label(label)
        .single_line()
        .w_full()
        .h(40.0)
        .px(11.0)
        .py(10.0)
        .radius(6.0)
        .fill(p.field)
        .border(1.0, p.line)
        .color(p.ink)
        .placeholder(placeholder, p.muted)
}

fn labeled(p: &Palette, label: &'static str, control: El) -> El {
    col()
        .w_full()
        .gap(6.0)
        .child(text(label).font(LABEL).color(p.muted))
        .child(control)
}
