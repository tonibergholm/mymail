mod gmail;
#[cfg(target_os = "macos")]
mod web;

use gmail::{Client, Message, OAuthClient, SavedAccount, Summary};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable, Theme, h_flex, h_resizable, resizable_panel, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    New,
    Reply,
    Forward,
}

enum Attach {
    /// A file picked from disk.
    Local(std::path::PathBuf),
    /// An attachment of the forwarded message, fetched from Gmail at send time.
    Original(gmail::Attachment),
}

impl Attach {
    fn name(&self) -> String {
        match self {
            Attach::Local(p) => p
                .file_name()
                .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned()),
            Attach::Original(a) => a.name.clone(),
        }
    }
}

/// Gmail's cap on attachments per message.
const MAX_ATTACH: u64 = 25 << 20;

struct Compose {
    kind: Kind,
    /// The message replied to or forwarded; None for new mail.
    msg: Option<Arc<Message>>,
    to: Entity<InputState>,
    subject: Entity<InputState>,
    body: Entity<TextareaState>,
    files: Vec<Attach>,
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
    html_view: Entity<TextViewState>,
    /// Native web view for HTML mail (macOS); gpui-kit's TextView elsewhere.
    #[cfg(target_os = "macos")]
    web: Option<std::rc::Rc<web::Web>>,
    compose: Option<Compose>,
    status: SharedString,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Bumped whenever `list` is replaced, so late callbacks can tell it changed.
    list_gen: u64,
    /// Archived this session; filtered from list results fetched before Gmail caught up.
    archived: std::collections::HashSet<String>,
    /// Mailbox historyId at the last inbox load; polling refreshes when it moves.
    history: Option<String>,
    /// A background check is in flight; the timer skips ticks until it ends.
    polling: bool,
    /// Error shown by a background run; the next successful one clears it.
    quiet_err: Option<SharedString>,
    _subs: Vec<Subscription>,
}

impl MailApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search (Gmail syntax, Enter)  — empty = Inbox")
        });
        let reader = cx.new(|cx| TextareaState::new(window, cx).soft_wrap(true));
        let html_view = cx.new(|cx| {
            TextViewState::html("", cx)
                .scrollable(true)
                .selectable(true)
        });
        Theme::sync_system_appearance(Some(window), cx);
        let subs = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx)
            }),
            cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.refresh(false, window, cx);
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
            html_view,
            #[cfg(target_os = "macos")]
            web: web::Web::new(window).map(std::rc::Rc::new),
            compose: None,
            status,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            list_gen: 0,
            archived: Default::default(),
            history: None,
            polling: false,
            quiet_err: None,
            _subs: subs,
        };
        if app.clients.is_empty() && app.oauth.is_some() {
            app.status = "No accounts yet. Use the account menu to sign in with Google.".into();
        } else {
            app.switch_account(0, window, cx);
        }
        app.focus.focus(window, cx);
        cx.background_spawn(async { gmail::prune_cache() }).detach();
        let secs = gmail::poll_secs();
        if secs > 0 {
            cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(secs))
                        .await;
                    let alive = this.update_in(cx, |this, window, cx| {
                        // Only the inbox view auto-updates; a search result stays put.
                        if !this.polling && this.search.read(cx).value().trim().is_empty() {
                            this.refresh(true, window, cx);
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        app
    }

    /// Typed text in an open composer that navigating away would throw away.
    fn draft_dirty(&self, cx: &App) -> bool {
        self.compose.as_ref().is_some_and(|c| {
            let typed = |s: &Entity<InputState>| !s.read(cx).value().trim().is_empty();
            !c.body.read(cx).value().trim().is_empty()
                || (c.kind == Kind::New && typed(&c.subject))
                || (c.kind != Kind::Reply && typed(&c.to))
                || c.files.iter().any(|f| matches!(f, Attach::Local(_)))
        })
    }

    /// Refuse an action that would discard a typed draft.
    fn keep_draft(&mut self, cx: &mut Context<Self>) -> bool {
        let dirty = self.draft_dirty(cx);
        if dirty {
            self.status = "Send or cancel the draft first.".into();
            cx.notify();
        }
        dirty
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
        // An empty label is background work: it only reports errors.
        let quiet = label.is_empty();
        if !quiet {
            self.status = label.to_string().into();
            self.quiet_err = None; // the status now belongs to this action
            cx.notify();
        }
        let task = cx.background_spawn(async move { job() });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                match &result {
                    Ok(_) if quiet => {
                        if this.quiet_err.take().is_some_and(|e| e == this.status) {
                            this.status = SharedString::default();
                        }
                    }
                    Ok(_) => this.status = SharedString::default(),
                    Err(e) => {
                        this.status = e.clone().into();
                        if quiet {
                            this.quiet_err = Some(this.status.clone());
                        }
                    }
                }
                done(this, result, window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `quiet`: a background update that keeps the selection and open message,
    /// and skips the reload when the mailbox hasn't changed since the last one.
    fn refresh(&mut self, quiet: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client() else { return };
        let q = self.search.read(cx).value().trim().to_string();
        let q = if q.is_empty() {
            "in:inbox".to_string()
        } else {
            q
        };
        let inbox = q == "in:inbox";
        self.polling |= quiet;
        let last = self.history.clone().filter(|_| quiet && inbox);
        let (account, query) = (client.clone(), q.clone());
        self.run(
            if quiet { "" } else { "Loading…" },
            window,
            cx,
            move || {
                // Read the history id first so a change during the list fetch isn't missed.
                let h = if inbox {
                    client.history_id().ok()
                } else {
                    None
                };
                if last.is_some() && h == last {
                    return Ok(None);
                }
                let (list, complete) = client.list(&q)?;
                if inbox && complete {
                    gmail::save_inbox(&client.email, &list);
                }
                // A partial load must not count as in sync, or polls would skip the retry.
                Ok(Some((h.filter(|_| complete), list)))
            },
            move |this, r, window, cx| {
                if quiet {
                    this.polling = false;
                }
                let Ok(Some((h, list))) = r else { return };
                // Drop stale results if the user switched account or query meanwhile.
                let now = this.search.read(cx).value().trim().to_string();
                if !this.client().is_some_and(|c| Arc::ptr_eq(&c, &account))
                    || (now != query && !(now.is_empty() && inbox))
                {
                    return;
                }
                if inbox {
                    this.history = h;
                }
                let keep = this
                    .selected
                    .and_then(|i| this.list.get(i))
                    .map(|x| x.id.clone());
                this.list = list;
                if inbox {
                    // An inbox fetch that began before an archive may still contain it.
                    this.list.retain(|x| !this.archived.contains(&x.id));
                }
                this.list_gen += 1;
                let draft_of = this
                    .compose
                    .as_ref()
                    .map(|c| c.msg.as_ref().map(|m| m.id.clone()));
                if quiet || draft_of.is_some() {
                    // Don't throw away a draft or yank the reader; just re-point the selection.
                    let id = draft_of.flatten().or(keep);
                    // Never auto-open here: opening marks the message read.
                    this.selected = id.and_then(|id| this.list.iter().position(|x| x.id == id));
                    return;
                }
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
        if self.keep_draft(cx) {
            return;
        }
        let s = self.list[ix].clone();
        self.compose = None;
        self.open = None; // actions must not target the previous message while this one loads
        self.run(
            "Opening…",
            window,
            cx,
            {
                let client = client.clone();
                let id = s.id.clone();
                move || client.message(&id)
            },
            move |this, r, window, cx| {
                let Ok(m) = r else { return };
                if this.selected.and_then(|i| this.list.get(i)).map(|x| &x.id) != Some(&m.id) {
                    return; // user moved on
                }
                if s.unread {
                    // Separate request, so a cached message shows without waiting for the network.
                    let id = s.id.clone();
                    this.run(
                        "",
                        window,
                        cx,
                        move || client.modify(&id, &["UNREAD"]),
                        move |this, r, _, _| match r {
                            Ok(()) => {
                                if let Some(x) = this.list.iter_mut().find(|x| x.id == s.id) {
                                    x.unread = false;
                                }
                            }
                            Err(_) => this.status = "Couldn't mark as read.".into(),
                        },
                    );
                }
                match &m.html {
                    #[cfg(target_os = "macos")]
                    Some(h) if this.web.is_some() => {
                        this.web.as_ref().unwrap().load(h, &details_html(&m))
                    }
                    // Fresh state per message so it opens scrolled to the top.
                    Some(h) => {
                        let h = format!("{}{h}", details_html(&m));
                        this.html_view = cx.new(|cx| {
                            TextViewState::html(&h, cx)
                                .scrollable(true)
                                .selectable(true)
                        })
                    }
                    None => this.reader.update(cx, |r, cx| {
                        r.set_value(format!("{}{}", details_text(&m), m.body), window, cx)
                    }),
                }
                this.open = Some(Arc::new(m));
            },
        );
    }

    fn select_and_open(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.keep_draft(cx) {
            return;
        }
        if ix < self.list.len() {
            self.select(ix, cx);
            self.open_selected(window, cx);
        }
    }

    fn archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(ix), Some(client)) = (self.selected, self.client()) else {
            return;
        };
        if self.keep_draft(cx) {
            return;
        }
        let (summary, list_gen) = (self.list.remove(ix), self.list_gen);
        let id = summary.id.clone();
        self.archived.insert(id.clone());
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
                if r.is_err() {
                    this.archived.remove(&summary.id);
                }
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
        if self.keep_draft(cx) {
            return;
        }
        let msg = self.open.clone();
        let to_value = match (kind, &msg) {
            (Kind::New, _) => String::new(),
            (_, None) => return,
            (_, Some(msg)) => match kind {
                // Follow-up on my own sent mail goes to the original recipients.
                Kind::Reply
                    if self.client().is_some_and(|c| {
                        gmail::address(&msg.from).eq_ignore_ascii_case(&c.email)
                    }) =>
                {
                    msg.to.clone()
                }
                Kind::Reply if !msg.reply_to.is_empty() => msg.reply_to.clone(),
                Kind::Reply => msg.from.clone(),
                _ => String::new(),
            },
        };
        let to = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("To");
            s.set_value(to_value, window, cx);
            s
        });
        let subject = cx.new(|cx| InputState::new(window, cx).placeholder("Subject"));
        let body = cx.new(|cx| TextareaState::new(window, cx).soft_wrap(true));
        match kind {
            Kind::Reply => body.update(cx, |b, cx| b.focus(window, cx)),
            _ => to.update(cx, |t, cx| t.focus(window, cx)),
        }
        let files = match (&msg, kind) {
            (Some(m), Kind::Forward) => m
                .attachments
                .iter()
                .cloned()
                .map(Attach::Original)
                .collect(),
            _ => vec![],
        };
        self.compose = Some(Compose {
            kind,
            msg: if kind == Kind::New { None } else { msg },
            to,
            subject,
            body,
            files,
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
        let subject = c.subject.read(cx).value().trim().to_string();
        let (kind, msg) = (c.kind, c.msg.clone());
        let picked: Vec<(
            String,
            Option<std::path::PathBuf>,
            Option<gmail::Attachment>,
        )> = c
            .files
            .iter()
            .map(|f| match f {
                Attach::Local(p) => (f.name(), Some(p.clone()), None),
                Attach::Original(a) => (f.name(), None, Some(a.clone())),
            })
            .collect();
        c.sending = true;
        let draft = c.body.entity_id();
        self.run(
            "Sending…",
            window,
            cx,
            move || {
                let mut files = vec![];
                let mut total = 0;
                for (name, path, original) in picked {
                    let data = match (path, &original, &msg) {
                        (Some(p), _, _) => std::fs::read(&p).map_err(|e| format!("{name}: {e}"))?,
                        (_, Some(a), Some(m)) => client.attachment(&m.id, &a.part)?,
                        _ => continue,
                    };
                    total += data.len() as u64;
                    if total > MAX_ATTACH {
                        return Err("Attachments are over Gmail's 25 MB limit.".into());
                    }
                    let mime = original
                        .map(|a| a.mime)
                        .unwrap_or_else(|| gmail::guess_mime(&name).into());
                    files.push(gmail::File { name, mime, data });
                }
                match (kind, msg) {
                    (Kind::Reply, Some(msg)) => client.send(
                        &gmail::reply_raw(&client.email, &to, &msg, &text, &files),
                        Some(&msg.thread_id),
                    ),
                    (Kind::Forward, Some(msg)) => client.send(
                        &gmail::forward_raw(&client.email, &to, &msg, &text, &files),
                        None,
                    ),
                    _ => client.send(
                        &gmail::new_raw(&client.email, &to, &subject, &text, &files),
                        None,
                    ),
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

    fn attach_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if let Some(c) = this.compose.as_mut().filter(|c| !c.sending) {
                    for p in paths {
                        if !c
                            .files
                            .iter()
                            .any(|f| matches!(f, Attach::Local(q) if *q == p))
                        {
                            c.files.push(Attach::Local(p));
                        }
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Save an attachment of the open message to Downloads and open it.
    fn download(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(m), Some(client)) = (self.open.clone(), self.client()) else {
            return;
        };
        let Some(a) = m.attachments.get(ix).cloned() else {
            return;
        };
        self.run(
            &format!("Downloading {}…", a.name),
            window,
            cx,
            move || gmail::save_download(&a.name, &client.attachment(&m.id, &a.part)?),
            |this, r, _, cx| {
                if let Ok(path) = r {
                    this.status = format!("Saved to {}", path.display()).into();
                    cx.open_with_system(&path);
                }
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
                // Replace in place: indexes (and `current`) must stay valid if the switch is refused.
                let client = Arc::new(Client::new(oauth, acct.clone()));
                let i = match this.saved.iter().position(|a| a.email == acct.email) {
                    Some(i) => {
                        this.saved[i] = acct;
                        this.clients[i] = client;
                        i
                    }
                    None => {
                        this.saved.push(acct);
                        this.clients.push(client);
                        this.clients.len() - 1
                    }
                };
                if let Err(e) = gmail::save_accounts(&this.saved) {
                    this.status = format!("Signed in, but saving failed: {e}").into();
                }
                this.switch_account(i, window, cx);
            },
        );
    }

    /// Clear everything bound to the old account before loading the new one.
    fn switch_account(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.keep_draft(cx) {
            return;
        }
        self.current = i;
        self.list_gen += 1;
        self.selected = None;
        self.open = None;
        self.compose = None;
        self.history = None;
        // Show the last known inbox at once (opening from the message cache), then update it.
        // The cache only stands in for the inbox, not for an active search.
        let inbox = self.search.read(cx).value().trim().is_empty();
        self.list = self
            .client()
            .filter(|_| inbox)
            .map(|c| gmail::cached_inbox(&c.email))
            .unwrap_or_default();
        self.list.retain(|x| !self.archived.contains(&x.id));
        let cached = !self.list.is_empty();
        self.select_and_open(0, window, cx);
        self.refresh(cached, window, cx);
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
            "c" => self.start_compose(Kind::New, window, cx),
            "/" => self.search.update(cx, |s, cx| s.focus(window, cx)),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        let s = &self.list[ix];
        let selected = self.selected == Some(ix);
        let read = t.foreground.opacity(0.6);
        v_flex()
            .id(ix)
            .w_full()
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
                    .items_center()
                    // Unread: blue dot and bold; read: dimmed, so the two differ in dark mode too.
                    .child(
                        div()
                            .size(px(7.))
                            .flex_none()
                            .rounded_full()
                            .when(s.unread, |d| d.bg(t.blue)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .when(s.unread, |d| d.font_weight(FontWeight::BOLD))
                            .when(!s.unread, |d| d.text_color(read))
                            .child(s.from.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t.muted_foreground)
                            .child(list_time(s.time, &s.date)),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .when(s.unread, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .when(!s.unread, |d| d.text_color(read))
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
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            // Step aside while the split is dragged, or the native view swallows the drag.
            web.set_visible(
                !cx.has_active_drag()
                    && !self.compose.as_ref().is_some_and(|c| c.kind == Kind::New)
                    && self.open.as_ref().is_some_and(|m| m.html.is_some()),
            );
        }
        if let Some(c) = self.compose.as_ref().filter(|c| c.kind == Kind::New) {
            // New mail takes the whole reader pane.
            return v_flex()
                .flex_1()
                .min_h_0()
                .child(self.render_compose(c, cx))
                .into_any_element();
        }
        let t = cx.theme();
        let Some(m) = self.open.clone() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.muted_foreground)
                .child("j/k move · e archive · r reply · f forward · c new · / search")
                .into_any_element();
        };
        // Minimal fixed bar; From/To/Date live at the top of the scrolling content.
        let act =
            |id: &'static str, label: &'static str| Button::new(id).ghost().xsmall().label(label);
        let mut col =
            v_flex().flex_1().min_h_0().child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(t.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(m.subject.clone()),
                            )
                            // Real sender stays in native UI: mail CSS can restyle the in-content header.
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(t.muted_foreground)
                                    .child(gmail::decode_words(&m.from)),
                            ),
                    )
                    .child(act("reply", "Reply").on_click(
                        cx.listener(|this, _, w, cx| this.start_compose(Kind::Reply, w, cx)),
                    ))
                    .child(act("fwd", "Forward").on_click(
                        cx.listener(|this, _, w, cx| this.start_compose(Kind::Forward, w, cx)),
                    ))
                    .child(
                        act("archive", "Archive")
                            .on_click(cx.listener(|this, _, w, cx| this.archive(w, cx))),
                    ),
            );
        if !m.attachments.is_empty() {
            let t = cx.theme();
            col = col.child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(t.border)
                    .children(m.attachments.iter().enumerate().map(|(i, a)| {
                        Button::new(("att", i))
                            .ghost()
                            .xsmall()
                            .label(format!("📎 {}  {}", a.name, human_size(a.size)))
                            .on_click(cx.listener(move |this, _, w, cx| this.download(i, w, cx)))
                    })),
            );
        }
        if let Some(c) = &self.compose {
            col = col.child(self.render_compose(c, cx));
        }
        #[cfg(target_os = "macos")]
        if let (Some(web), true) = (self.web.clone(), m.html.is_some()) {
            let pane = canvas(|_, _, _| (), move |bounds, _, _, _| web.place(bounds)).size_full();
            return col
                .child(div().flex_1().min_h_0().min_w_0().child(pane))
                .into_any_element();
        }
        let content = if m.html.is_some() {
            TextView::new(&self.html_view)
                .size_full()
                .into_any_element()
        } else {
            Textarea::new(&self.reader)
                .readonly(true)
                .bordered(false)
                .h(relative(1.))
                .into_any_element()
        };
        col.child(
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .p_3()
                .child(content),
        )
        .into_any_element()
    }
}

impl MailApp {
    fn render_compose(&self, c: &Compose, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        let title = match c.kind {
            Kind::New => "New message",
            Kind::Reply => "Reply",
            Kind::Forward => "Forward",
        };
        let new = c.kind == Kind::New;
        v_flex()
            .p_3()
            .gap_2()
            .border_b_1()
            .border_color(t.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(Input::new(&c.to).disabled(c.sending))
            .when(new, |d| d.child(Input::new(&c.subject).disabled(c.sending)))
            .child(
                Textarea::new(&c.body)
                    .h(px(if new { 320. } else { 180. }))
                    .disabled(c.sending),
            )
            .when(!c.files.is_empty(), |d| {
                d.child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(c.files.iter().enumerate().map(|(i, f)| {
                            Button::new(("file", i))
                                .outline()
                                .xsmall()
                                .disabled(c.sending)
                                .label(format!("{}  ×", f.name()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(c) = this.compose.as_mut() {
                                        c.files.remove(i);
                                        cx.notify();
                                    }
                                }))
                        })),
                )
            })
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
                        Button::new("attach")
                            .ghost()
                            .small()
                            .disabled(c.sending)
                            .label("Attach…")
                            .on_click(cx.listener(|this, _, w, cx| this.attach_files(w, cx))),
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
            )
    }
}

/// Header lines shown at the top of the message, scrolling away with it.
fn details(m: &Message) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("From", gmail::decode_words(&m.from)),
        ("To", gmail::decode_words(&m.to)),
    ];
    if !m.cc.is_empty() {
        v.push(("Cc", gmail::decode_words(&m.cc)));
    }
    v.push((
        "Date",
        local(m.time).map_or_else(
            || m.date.clone(),
            |t| t.format("%a %-d %b %Y %H:%M").to_string(),
        ),
    ));
    v
}

fn details_text(m: &Message) -> String {
    details(m)
        .iter()
        .map(|(k, v)| format!("{k}: {v}\n"))
        .collect::<String>()
        + "\n"
}

fn details_html(m: &Message) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let rows: String = details(m)
        .iter()
        .map(|(k, v)| {
            format!(
                "<div><span style=\"color:#888\">{k}</span>&nbsp; {}</div>",
                esc(v)
            )
        })
        .collect();
    format!(
        "<div style=\"font:12px/1.6 -apple-system,system-ui,sans-serif;color:#333;background:#fff;\
         padding:8px 14px;border-bottom:1px solid #e5e5e5;text-align:left\">{rows}</div>"
    )
}

fn human_size(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{} KB", n / 1024),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

fn local(ms: i64) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    (ms > 0)
        .then(|| chrono::Local.timestamp_millis_opt(ms).single())
        .flatten()
}

/// Local time: "14:05" today, "4 Nov" this year, "4 Nov 2025" before.
fn list_time(ms: i64, header: &str) -> String {
    let Some(t) = local(ms) else {
        // No internalDate from Gmail: "Tue, 4 Nov 2026 10:22:01 +0200" -> "4 Nov"
        let d = header.split_once(", ").map_or(header, |x| x.1);
        return d.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    };
    format_list_time(t, chrono::Local::now())
}

fn format_list_time<Tz: chrono::TimeZone>(
    t: chrono::DateTime<Tz>,
    now: chrono::DateTime<Tz>,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    use chrono::Datelike;
    let fmt = if t.date_naive() == now.date_naive() {
        "%H:%M"
    } else if t.year() == now.year() {
        "%-d %b"
    } else {
        "%-d %b %Y"
    };
    t.format(fmt).to_string()
}

impl Render for MailApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let this = cx.entity().downgrade();
        let emails: Vec<String> = self.clients.iter().map(|c| c.email.clone()).collect();
        let (current, can_add) = (self.current, self.oauth.is_some());
        let label = emails
            .get(current)
            .cloned()
            .unwrap_or_else(|| "Accounts".into());
        let accounts = Button::new("accounts")
            .ghost()
            .small()
            .label(format!("{label} ▾"))
            .dropdown_menu(move |mut menu, _, _| {
                for (i, email) in emails.iter().enumerate() {
                    let this = this.clone();
                    menu = menu.item(
                        PopupMenuItem::new(email.clone())
                            .checked(i == current)
                            .on_click(move |_, window, cx| {
                                this.update(cx, |app, cx| {
                                    if app.current != i {
                                        app.switch_account(i, window, cx)
                                    }
                                })
                                .ok();
                            }),
                    );
                }
                if can_add {
                    let this = this.clone();
                    menu = menu
                        .separator()
                        .item(
                            PopupMenuItem::new("Add account…").on_click(move |_, window, cx| {
                                this.update(cx, |app, cx| app.add_account(window, cx)).ok();
                            }),
                        );
                }
                menu
            });
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
                    .child(Button::new("new").ghost().small().label("New").on_click(
                        cx.listener(|this, _, w, cx| this.start_compose(Kind::New, w, cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.search).cleanable(true)),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("split")
                        .child(
                            resizable_panel()
                                .size(px(380.))
                                .size_range(px(240.)..px(900.))
                                .child(
                                    div()
                                        .size_full()
                                        .track_focus(&self.focus)
                                        .on_key_down(cx.listener(Self::on_key))
                                        .child(
                                            uniform_list(
                                                "messages",
                                                count,
                                                cx.processor(
                                                    |this, range: std::ops::Range<usize>, _, cx| {
                                                        range
                                                            .map(|ix| this.render_row(ix, cx))
                                                            .collect::<Vec<_>>()
                                                    },
                                                ),
                                            )
                                            .track_scroll(&self.scroll)
                                            .size_full(),
                                        ),
                                ),
                        )
                        .child(
                            resizable_panel().child(
                                div()
                                    .size_full()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(self.render_detail(cx)),
                            ),
                        ),
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
            // Lets HTML mail load remote images.
            cx.set_http_client(Arc::new(reqwest_client::ReqwestClient::new()));
            cx.on_window_closed(|cx, _| cx.quit()).detach();
            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    app_id: Some("mymail".into()), // matches the Linux .desktop entry
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

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone};

    #[test]
    fn list_time_is_relative_to_today() {
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = tz.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap();
        let at = |y, mo, d, h, mi| tz.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap();
        assert_eq!(
            super::format_list_time(at(2026, 11, 11, 9, 5), now),
            "09:05"
        );
        assert_eq!(super::format_list_time(at(2026, 11, 4, 9, 5), now), "4 Nov");
        assert_eq!(
            super::format_list_time(at(2025, 12, 31, 9, 5), now),
            "31 Dec 2025"
        );
    }
}
