use axum::http::StatusCode;

use super::route_table::RouteInfo;

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn render_error_page(status: StatusCode, title: &str, message: &str, details: Option<&str>) -> String {
    let details_html = details
        .map(|d| format!("<pre>{}</pre>", escape_html(d)))
        .unwrap_or_default();

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{code} {title}</title><style>body{{font-family:ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif;background:#f7f7f8;color:#111;margin:0}}main{{max-width:720px;margin:8vh auto;padding:24px;background:#fff;border:1px solid #e5e7eb;border-radius:12px}}h1{{margin:0 0 8px;font-size:28px}}p{{line-height:1.5}}pre{{background:#f3f4f6;border:1px solid #e5e7eb;border-radius:8px;padding:12px;overflow:auto}}ul{{padding-left:20px}}code{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}}</style></head><body><main><h1>{code} {title}</h1><p>{message}</p>{details}</main></body></html>",
        code = status.as_u16(),
        title = escape_html(title),
        message = escape_html(message),
        details = details_html,
    )
}

pub fn render_not_found_for_route(hostname: &str, routes: &[RouteInfo]) -> String {
    let safe_host = escape_html(hostname);
    let hint = hostname.strip_suffix(".localhost").unwrap_or(hostname);
    let safe_hint = escape_html(hint);

    let routes_html = if routes.is_empty() {
        "<p><em>No apps running.</em></p>".to_string()
    } else {
        let items = routes
            .iter()
            .map(|r| {
                format!(
                    "<li><a href=\"{url}\">{host}</a> - localhost:{port}</li>",
                    url = escape_html(&r.url),
                    host = escape_html(&r.hostname),
                    port = r.target_port
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!("<p><strong>Active apps</strong></p><ul>{}</ul>", items)
    };

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>404 Not Found</title><style>body{{font-family:ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif;background:#f7f7f8;color:#111;margin:0}}main{{max-width:720px;margin:8vh auto;padding:24px;background:#fff;border:1px solid #e5e7eb;border-radius:12px}}h1{{margin:0 0 8px;font-size:28px}}p{{line-height:1.5}}code{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}}</style></head><body><main><h1>404 Not Found</h1><p>No app registered for <strong>{host}</strong>.</p>{routes}<p>Start an app with: <code>hostless run {hint} -- &lt;command&gt;</code></p></main></body></html>",
        host = safe_host,
        routes = routes_html,
        hint = safe_hint,
    )
}
