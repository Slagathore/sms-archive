//! Pure vCard/CSV string formatting helpers (escaping, line unfolding,
//! property parsing, N/ADR field formatting). No I/O or app state.

pub(crate) fn vcard_escape(value: &str) -> String {
    let mut out = value.replace('\\', "\\\\");
    out = out.replace('\n', "\\n");
    out = out.replace(';', "\\;");
    out = out.replace(',', "\\,");
    out
}

pub(crate) fn vcard_unescape(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    'n' | 'N' => out.push('\n'),
                    ',' => out.push(','),
                    ';' => out.push(';'),
                    '\\' => out.push('\\'),
                    other => out.push(other),
                }
            } else {
                out.push('\\');
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn unfold_vcard_lines(contents: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in contents.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            if let Some(last) = lines.last_mut() {
                last.push_str(raw.trim_start());
            }
        } else {
            lines.push(raw.trim_end().to_string());
        }
    }
    lines
}

pub(crate) fn parse_vcard_property(raw: &str) -> (String, Vec<String>) {
    let mut parts = raw.split(';');
    let name = parts.next().unwrap_or("").trim().to_ascii_uppercase();
    let mut types = Vec::new();
    for part in parts {
        let mut iter = part.splitn(2, '=');
        let key = iter.next().unwrap_or("").trim().to_ascii_lowercase();
        let value = iter.next().unwrap_or("").trim();
        if key == "type" {
            for t in value.split(',') {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    types.push(trimmed.to_ascii_lowercase());
                }
            }
        }
    }
    (name, types)
}

pub(crate) fn vcard_phone_type_label(value: &str) -> &'static str {
    match value {
        "mobile" => "CELL",
        "home" => "HOME",
        "work" => "WORK",
        _ => "VOICE",
    }
}

pub(crate) fn vcard_phone_type_from_params(types: &[String]) -> String {
    for t in types {
        match t.as_str() {
            "cell" | "mobile" => return "mobile".to_string(),
            "home" => return "home".to_string(),
            "work" => return "work".to_string(),
            _ => {}
        }
    }
    String::new()
}

pub(crate) fn format_vcard_name(value: &str) -> String {
    let parts: Vec<&str> = value.split(';').collect();
    let family = parts.first().copied().unwrap_or_default();
    let given = parts.get(1).copied().unwrap_or_default();
    let additional = parts.get(2).copied().unwrap_or_default();
    let prefix = parts.get(3).copied().unwrap_or_default();
    let suffix = parts.get(4).copied().unwrap_or_default();
    let mut out = Vec::new();
    for part in [prefix, given, additional, family, suffix] {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
    }
    out.join(" ")
}

pub(crate) fn format_vcard_address(value: &str) -> String {
    let parts: Vec<&str> = value.split(';').collect();
    let mut out = Vec::new();
    for part in parts {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
    }
    out.join(" ")
}
