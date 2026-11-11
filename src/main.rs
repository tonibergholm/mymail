mod gmail;

use gmail::{Client, Message, OAuthClient, SavedAccount, Summary};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable, Theme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Reply,
    Forward,
}

struct Compose {
    kind: Kind,
    msg: Arc<Message>,
    to: Entity<InputState>,
    body: Entity<TextareaState>,
    sending: bool,
}

struct MailApp {
    oauth: Option<OAuthClient>,
    saved: Vec<SavedAccount>,
    clients: Vec<Arc<Client>>,
    current: usize,
    search: Entity<InputState>,
    list: Vec<Summary>,
    selected: Option<usize>,
    open: Option<Arc<Message>>,
    reader: Entity<TextareaState>,
    compose: Option<Compose>,
    status: SharedString,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Bumped whenever `list` is replaced, so late callbacks can tell it changed.
    list_gen: u64,
    _subs: Vec<Subscription>,
}

impl MailApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search (Gmail syntax, Enter)  — empty = Inbox")
        });
        let reader = cx.new(|cx| TextareaState::new(window, cx).soft_wrap(true));
        Theme::sync_system_appearance(Some(window), cx);
        let subs = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx)
            }),
            cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.refresh(window, cx);
                    this.focus.focus(window, cx);
                }
            }),
        ];
        let (oauth, status) = match gmail::load_client() {
            Ok(c) => (Some(c), SharedString::default()),
            Err(e) => (None, e.into()),
        };
        let saved = gmail::load_accounts();
        let clients = oauth
            .as_ref()
            .map(|o| {
                saved
                    .iter()
                    .map(|a| Arc::new(Client::new(o.clone(), a.clone())))
                    .collect()
            })
            .unwrap_or_default();
        let mut app = MailApp {
            oauth,
            saved,
            clients,
            current: 0,
            search,
            list: vec![],
            selected: None,
            open: None,
            reader,
            compose: None,
            status,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            list_gen: 0,
            _subs: subs,
        };
        if app.clients.is_empty() && app.oauth.is_some() {
            app.status = "No accounts yet. Click “+ Account” to sign in with Google.".into();
        } else {
            app.refresh(window, cx);
        }
        app.focus.focus(window, cx);
        app
    }

    fn client(&self) -> Option<Arc<Client>> {
        self.clients.get(self.current).cloned()
    }

    /// Run blocking `job` off the UI thread, then apply `done` on it.
    fn run<T: Send + 'static>(
        &mut self,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce() -> gmail::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, gmail::Result<T>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.status = label.to_string().into();
        cx.notify();
        let task = cx.background_spawn(async move { job() });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.status = match &result {
                    Ok(_) => SharedString::default(),
                    Err(e) => e.clone().into(),
                };
                done(this, result, window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client() else { return };
        let q = self.search.read(cx).value().trim().to_string();
        let q = if q.is_empty() {
            "in:inbox".to_string()
        } else {
            q
        };
        let (account, query) = (self.current, q.clone());
        self.run(
            "Loading…",
            window,
            cx,
            move || client.list(&q),
            move |this, list, window, cx| {
                let Ok(list) = list else { return };
                // Drop stale results if the user switched account or query meanwhile.
                let now = this.search.read(cx).value().trim().to_string();
                if this.current != account
                    || (now != query && !(now.is_empty() && query == "in:inbox"))
                {
                    return;
                }
                this.list = list;
                this.list_gen += 1;
                this.selected = None;
                this.open = None;
                this.compose = None;
                this.select_and_open(0, window, cx);
            },
        );
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.list.len() {
            return;
        }
        self.selected = Some(ix);
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        cx.notify();
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(ix), Some(client)) = (self.selected, self.client()) else {
            return;
        };
        let s = self.list[ix].clone();
        self.compose = None;
        self.open = None; // actions must not target the previous message while this one loads
        self.run(
            "Opening…",
            window,
            cx,
            move || {
                let m = client.message(&s.id)?;
                let marked = !s.unread || client.modify(&s.id, &["UNREAD"]).is_ok();
                Ok((m, marked))
            },
            move |this, r, window, cx| {
                let Ok((m, marked)) = r else { return };
                if this.selected.and_then(|i| this.list.get(i)).map(|x| &x.id) != Some(&m.id) {
                    return; // user moved on
                }
                if !marked {
                    this.status = "Couldn't mark as read.".into();
                } else if let Some(x) = this.list.iter_mut().find(|x| x.id == m.id) {
                    x.unread = false;
                }
                this.reader
                    .update(cx, |r, cx| r.set_value(m.body.clone(), window, cx));
                this.open = Some(Arc::new(m));
            },
        );
    }

    fn select_and_open(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.list.len() {
            self.select(ix, cx);
            self.open_selected(window, cx);
        }
    }

    fn archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(ix), Some(client)) = (self.selected, self.client()) else {
            return;
        };
        let (summary, list_gen) = (self.list.remove(ix), self.list_gen);
        let id = summary.id.clone();
        self.open = None;
        self.compose = None;
        self.selected = None;
        if !self.list.is_empty() {
            self.select_and_open(ix.min(self.list.len() - 1), window, cx);
        }
        self.run(
            "Archiving…",
            window,
            cx,
            move || client.archive(&id),
            move |this, r, _, _| {
                // Optimistic removal: put the row back if Gmail refused.
                if r.is_err()
                    && this.list_gen == list_gen
                    && !this.list.iter().any(|x| x.id == summary.id)
                {
                    let at = ix.min(this.list.len());
                    this.list.insert(at, summary);
                    this.selected = this.selected.map(|s| if s >= at { s + 1 } else { s });
                }
            },
        );
    }

    fn start_compose(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(msg) = self.open.clone() else { return };
        let to_value = match kind {
            Kind::Reply if !msg.reply_to.is_empty() => msg.reply_to.clone(),
            Kind::Reply => msg.from.clone(),
            Kind::Forward => String::new(),
        };
        let to = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("To");
            s.set_value(to_value, window, cx);
            s
        });
        let body = cx.new(|cx| TextareaState::new(window, cx).soft_wrap(true));
        match kind {
            Kind::Reply => body.update(cx, |b, cx| b.focus(window, cx)),
            Kind::Forward => to.update(cx, |t, cx| t.focus(window, cx)),
        }
        self.compose = Some(Compose {
            kind,
            msg,
            to,
            body,
            sending: false,
        });
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client();
        let (Some(c), Some(client)) = (self.compose.as_mut(), client) else {
            return;
        };
        if c.sending {
            return;
        }
        let to = c.to.read(cx).value().trim().to_string();
        if !to.contains('@') {
            self.status = "Recipient missing.".into();
            return cx.notify();
        }
        let text = c.body.read(cx).value().to_string();
        let (kind, msg) = (c.kind, c.msg.clone());
        c.sending = true;
        let draft = c.body.entity_id();
        self.run(
            "Sending…",
            window,
            cx,
            move || match kind {
                Kind::Reply => client.send(
                    &gmail::reply_raw(&client.email, &to, &msg, &text),
                    Some(&msg.thread_id),
                ),
                Kind::Forward => {
                    client.send(&gmail::forward_raw(&client.email, &to, &msg, &text), None)
                }
            },
            move |this, r, window, cx| {
                // Only touch the composer that sent; another may have opened meanwhile.
                let Some(c) = this
                    .compose
                    .as_mut()
                    .filter(|c| c.body.entity_id() == draft)
                else {
                    return;
                };
                if r.is_err() {
                    c.sending = false;
                    return;
                }
                this.compose = None;
                this.status = "Sent.".into();
                this.focus.focus(window, cx);
            },
        );
    }

    fn add_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(oauth) = self.oauth.clone() else {
            return;
        };
        let login = match gmail::Login::start(&oauth) {
            Ok(l) => l,
            Err(e) => {
                self.status = e.into();
                return cx.notify();
            }
        };
        cx.open_url(&login.url);
        let o = oauth.clone();
        self.run(
            "Waiting for Google sign-in in your browser…",
            window,
            cx,
            move || login.finish(&o),
            move |this, acct, window, cx| {
                let Ok(acct) = acct else { return };
                this.saved.retain(|a| a.email != acct.email);
                this.clients.retain(|c| c.email != acct.email);
                this.saved.push(acct.clone());
                this.clients.push(Arc::new(Client::new(oauth, acct)));
                if let Err(e) = gmail::save_accounts(&this.saved) {
                    this.status = format!("Signed in, but saving failed: {e}").into();
                }
                let last = this.clients.len() - 1;
                this.switch_account(last, window, cx);
            },
        );
    }

    /// Clear everything bound to the old account before loading the new one.
    fn switch_account(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.current = i;
        self.list.clear();
        self.list_gen += 1;
        self.selected = None;
        self.open = None;
        self.compose = None;
        self.refresh(window, cx);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.platform || m.control || m.alt {
            return; // leave Cmd+F, Ctrl+E etc. to the system
        }
        let next = self.selected.map_or(0, |i| i + 1);
        let prev = self.selected.map_or(0, |i| i.saturating_sub(1));
        match ev.keystroke.key.as_str() {
            "j" | "down" => self.select_and_open(next, window, cx),
            "k" | "up" => self.select_and_open(prev, window, cx),
            "enter" | "o" => self.open_selected(window, cx),
            "e" => self.archive(window, cx),
            "r" => self.start_compose(Kind::Reply, window, cx),
            "f" => self.start_compose(Kind::Forward, window, cx),
            "/" => self.search.update(cx, |s, cx| s.focus(window, cx)),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        let s = &self.list[ix];
        let selected = self.selected == Some(ix);
        v_flex()
            .id(ix)
            .h(px(76.))
            .px_3()
            .py_1()
            .justify_center()
            .border_b_1()
            .border_color(t.border)
            .when(selected, |d| d.bg(t.list_active))
            .when(!selected, |d| d.hover(|d| d.bg(t.list_hover)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .when(s.unread, |d| d.font_weight(FontWeight::BOLD))
                            .child(s.from.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t.muted_foreground)
                            .child(short_date(&s.date)),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .when(s.unread, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(s.subject.clone()),
            )
            .child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(t.muted_foreground)
                    .child(s.snippet.clone()),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_and_open(ix, window, cx);
                this.focus.focus(window, cx);
            }))
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let Some(m) = self.open.clone() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.muted_foreground)
                .child("j/k move · e archive · r reply · f forward · / search")
                .into_any_element();
        };
        let muted = t.muted_foreground;
        let line = |label: &str, value: &str| {
            h_flex()
                .gap_2()
                .child(div().w(px(48.)).text_color(muted).child(label.to_string()))
                .child(value.to_string())
        };
        let mut col = v_flex().flex_1().min_h_0().child(
            v_flex()
                .p_3()
                .gap_1()
                .border_b_1()
                .border_color(t.border)
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(m.subject.clone()),
                )
                .child(line("From", &m.from))
                .child(line("To", &m.to))
                .when(!m.cc.is_empty(), |d| d.child(line("Cc", &m.cc)))
                .child(line("Date", &m.date))
                .child(
                    h_flex()
                        .pt_1()
                        .gap_2()
                        .child(Button::new("reply").small().label("Reply (r)").on_click(
                            cx.listener(|this, _, w, cx| this.start_compose(Kind::Reply, w, cx)),
                        ))
                        .child(Button::new("fwd").small().label("Forward (f)").on_click(
                            cx.listener(|this, _, w, cx| this.start_compose(Kind::Forward, w, cx)),
                        ))
                        .child(
                            Button::new("archive")
                                .small()
                                .label("Archive (e)")
                                .on_click(cx.listener(|this, _, w, cx| this.archive(w, cx))),
                        ),
                ),
        );
        if let Some(c) = &self.compose {
            let title = if c.kind == Kind::Reply {
                "Reply"
            } else {
                "Forward"
            };
            col = col.child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(t.border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(Input::new(&c.to))
                    .child(Textarea::new(&c.body).h(px(180.)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("send")
                                    .primary()
                                    .small()
                                    .loading(c.sending)
                                    .label("Send")
                                    .on_click(cx.listener(|this, _, w, cx| this.send(w, cx))),
                            )
                            .child(
                                Button::new("cancel")
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.compose = None;
                                        this.focus.focus(w, cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }
        col.child(
            div().flex_1().min_h_0().p_2().child(
                Textarea::new(&self.reader)
                    .readonly(true)
                    .bordered(false)
                    .h(relative(1.)),
            ),
        )
        .into_any_element()
    }
}

/// "Tue, 4 Nov 2026 10:22:01 +0200" -> "4 Nov"
fn short_date(d: &str) -> String {
    let d = d.split_once(", ").map_or(d, |x| x.1);
    d.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

impl Render for MailApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let mut accounts = h_flex().gap_1();
        for (i, c) in self.clients.iter().enumerate() {
            let b = Button::new(("acct", i)).small().label(c.email.clone());
            let b = if i == self.current {
                b.primary()
            } else {
                b.ghost()
            };
            accounts = accounts.child(b.on_click(cx.listener(move |this, _, w, cx| {
                if this.current != i {
                    this.switch_account(i, w, cx);
                }
            })));
        }
        let count = self.list.len();
        v_flex()
            .size_full()
            .bg(t.background)
            .text_color(t.foreground)
            .text_sm()
            .child(
                h_flex()
                    .gap_2()
                    .p_2()
                    .border_b_1()
                    .border_color(t.border)
                    .child(accounts)
                    .when(self.oauth.is_some(), |d| {
                        d.child(
                            Button::new("add")
                                .ghost()
                                .small()
                                .label("+ Account")
                                .on_click(cx.listener(|this, _, w, cx| this.add_account(w, cx))),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.search).cleanable(true)),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        div()
                            .w(px(380.))
                            .h_full()
                            .border_r_1()
                            .border_color(t.border)
                            .track_focus(&self.focus)
                            .on_key_down(cx.listener(Self::on_key))
                            .child(
                                uniform_list(
                                    "messages",
                                    count,
                                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                        range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                                    }),
                                )
                                .track_scroll(&self.scroll)
                                .size_full(),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(self.render_detail(cx)),
                    ),
            )
            .when(!self.status.is_empty(), |d| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(t.border)
                        .text_xs()
                        .text_color(t.muted_foreground)
                        .child(self.status.clone()),
                )
            })
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            cx.on_window_closed(|cx, _| cx.quit()).detach();
            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("MyMail".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| MailApp::new(window, cx)),
            )
            .expect("open window");
            cx.activate(true);
        });
}
