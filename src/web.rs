//! macOS: show HTML mail in a native WKWebView child view, the engine Mail.app uses.
//! JavaScript is off; clicked links open in the default browser.

use gpui_kit::{Bounds, Pixels, Window};
use std::cell::Cell;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{NewWindowResponse, Rect, WebView, WebViewBuilder};

const INSET: f64 = 6.0;

pub struct Web {
    view: WebView,
    bounds: Cell<Option<Bounds<Pixels>>>,
    visible: Cell<bool>,
}

/// Untrusted mail may link anywhere; only hand web and mail links to the OS.
fn open_external(url: &str) {
    if url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:") {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
}

impl Web {
    pub fn new(window: &Window) -> Option<Self> {
        let view = WebViewBuilder::new()
            .with_javascript_disabled()
            .with_visible(false)
            .with_navigation_handler(|url| {
                // load_html navigates to about:blank; everything else is a click.
                if url.starts_with("about:") {
                    return true;
                }
                open_external(&url);
                false
            })
            .with_new_window_req_handler(|url, _| {
                open_external(&url);
                NewWindowResponse::Deny
            })
            .build_as_child(window)
            .ok()?;
        Some(Web {
            view,
            bounds: Cell::new(None),
            visible: Cell::new(false),
        })
    }

    /// `header` (trusted, ours) goes right after the CSP, above the mail.
    pub fn load(&self, html: &str, header: &str) {
        let _ = self.view.load_html(&harden(html, header));
        // Keep j/k/e/r/f going to the app, not the web view.
        let _ = self.view.focus_parent();
    }

    pub fn set_visible(&self, visible: bool) {
        if self.visible.replace(visible) != visible {
            let _ = self.view.set_visible(visible);
        }
    }

    /// Follow the reader pane; called from the pane's paint pass.
    pub fn place(&self, b: Bounds<Pixels>) {
        if self.bounds.replace(Some(b)) != Some(b) {
            let _ = self.view.set_bounds(Rect {
                // Leave the split handle's grab area (left edge) to GPUI.
                position: LogicalPosition::new(
                    f64::from(b.origin.x) + INSET,
                    f64::from(b.origin.y),
                )
                .into(),
                size: LogicalSize::new(f64::from(b.size.width) - INSET, f64::from(b.size.height))
                    .into(),
            });
        }
    }
}

/// Without JavaScript, mail can only navigate on its own through meta refresh or frames.
/// Block both (and forms) so only real link clicks reach `open_external`.
fn harden(html: &str, header: &str) -> String {
    const CSP: &str = "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
        img-src * data:; style-src * 'unsafe-inline'; font-src * data:; \
        frame-src 'none'; form-action 'none'; script-src 'none'\">";
    // Disable the mail's own http-equiv metas (refresh, a looser CSP); ours comes first.
    let lower = html.to_ascii_lowercase();
    let mut body = String::with_capacity(html.len());
    let mut last = 0;
    for (i, _) in lower.match_indices("http-equiv") {
        body += &html[last..i];
        body += "data-x-equiv";
        last = i + "http-equiv".len();
    }
    body += &html[last..];
    // Insert after a leading doctype: anything before it would force quirks mode.
    let start = body.len() - body.trim_start().len();
    let at = if body[start..].to_ascii_lowercase().starts_with("<!doctype") {
        body[start..].find('>').map_or(0, |e| start + e + 1)
    } else {
        0
    };
    body.insert_str(at, &format!("{CSP}{header}"));
    body
}

#[cfg(test)]
mod tests {
    #[test]
    fn harden_blocks_auto_navigation() {
        let h = super::harden(
            r#"<META HTTP-EQUIV="refresh" content="0;url=https://evil"><p>hi</p>"#,
            "",
        );
        assert!(h.starts_with("<meta http-equiv=\"Content-Security-Policy\""));
        assert!(h.contains("frame-src 'none'"));
        assert!(
            !h[h.find('>').unwrap()..]
                .to_ascii_lowercase()
                .contains("http-equiv")
        );
        assert!(h.ends_with("<p>hi</p>"));
    }

    #[test]
    fn harden_keeps_doctype_first() {
        let h = super::harden(
            "\n<!DOCTYPE html><html><body>x</body></html>",
            "<div>hdr</div>",
        );
        assert!(h.starts_with("\n<!DOCTYPE html><meta http-equiv=\"Content-Security-Policy\""));
    }
}
