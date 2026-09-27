//! TOML設定ファイルのコメントを保ったまま行う最小限の更新。
//!
//! 設定ファイルは配布テンプレートを人間が編集して使うため、TOMLを
//! 再シリアライズしてコメントや並びを失わせない。ここでは通常の
//! `[section]` とキー行だけを扱い、配列テーブル(`[[section]]`)は対象外とする。

/// TOMLへ書き込む値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TomlValue {
    Str(String),
    Bool(bool),
    Int(i64),
    StrArray(Vec<String>),
}

impl TomlValue {
    fn render(&self) -> String {
        match self {
            Self::Str(value) => format!("\"{}\"", escape_basic_string(value)),
            Self::Bool(value) => value.to_string(),
            Self::Int(value) => value.to_string(),
            Self::StrArray(values) => {
                let values = values
                    .iter()
                    .map(|value| format!("\"{}\"", escape_basic_string(value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{values}]")
            }
        }
    }
}

/// TOML basic stringとして安全に埋め込める形へ変換する。
pub fn escape_basic_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\u{08}' => escaped.push_str("\\b"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\u{0c}' => escaped.push_str("\\f"),
            '\r' => escaped.push_str("\\r"),
            ch if ch.is_control() => {
                use std::fmt::Write;
                write!(&mut escaped, "\\u{:04X}", ch as u32).expect("String write cannot fail");
            }
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn newline(line: &str) -> &str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn content(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
}

fn is_separator_comment(line: &str) -> bool {
    let trimmed = content(line).trim();
    trimmed.starts_with('#')
        && (trimmed.contains("===") || trimmed.contains("---") || trimmed.contains("***"))
}

fn section_name(line: &str) -> Option<&str> {
    let trimmed = content(line).trim();
    if trimmed.starts_with('[') && !trimmed.starts_with("[[") && trimmed.ends_with(']') {
        Some(trimmed.strip_prefix('[')?.strip_suffix(']')?.trim())
    } else {
        None
    }
}

fn commented_section_name(line: &str) -> Option<&str> {
    let trimmed = content(line).trim_start();
    let commented = trimmed.strip_prefix('#')?.trim_start();
    if commented.starts_with('[') && !commented.starts_with("[[") && commented.ends_with(']') {
        Some(commented.strip_prefix('[')?.strip_suffix(']')?.trim())
    } else {
        None
    }
}

fn assignment_key(line: &str) -> Option<&str> {
    let trimmed = content(line).trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let (lhs, _) = trimmed.split_once('=')?;
    let key = lhs.trim();
    (!key.is_empty() && !key.contains(' ')).then_some(key)
}

fn commented_assignment_key(line: &str) -> Option<&str> {
    let trimmed = content(line).trim_start();
    let commented = trimmed.strip_prefix('#')?.trim_start();
    let (lhs, _) = commented.split_once('=')?;
    let key = lhs.trim();
    (!key.is_empty() && !key.contains(' ')).then_some(key)
}

fn inline_comment(line: &str) -> Option<&str> {
    let body = content(line);
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
        } else if ch == '#' && !in_string {
            return Some(&body[index..]);
        }
    }
    None
}

fn line_with_newline(line: &str, replacement: &str) -> String {
    format!("{replacement}{}", newline(line))
}

fn indent(line: &str) -> &str {
    &content(line)[..content(line).len() - content(line).trim_start().len()]
}

fn active_section_range(lines: &[String], section: &str) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|line| section_name(line) == Some(section))?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| section_name(line).map(|_| index))
        .unwrap_or(lines.len());
    Some((start, end))
}

fn activate_commented_section(lines: &mut [String], section: &str) -> bool {
    let Some(index) = lines
        .iter()
        .position(|line| commented_section_name(line) == Some(section))
    else {
        return false;
    };
    let line = &lines[index];
    let body = content(line);
    let prefix_len = body.len() - body.trim_start().len();
    let prefix = &body[..prefix_len];
    let after_hash = body[prefix_len..].strip_prefix('#').unwrap_or_default();
    let after_hash = after_hash.trim_start();
    lines[index] = line_with_newline(line, &format!("{prefix}{after_hash}"));
    true
}

fn replace_in_section(lines: &mut [String], section: &str, key: &str, value: &str) -> bool {
    let Some((start, end)) = active_section_range(lines, section) else {
        return false;
    };
    for index in start + 1..end {
        if assignment_key(&lines[index]) == Some(key) {
            let comment = inline_comment(&lines[index])
                .map(|comment| format!(" {comment}"))
                .unwrap_or_default();
            let replacement = format!("{}{} = {value}{comment}", indent(&lines[index]), key);
            lines[index] = line_with_newline(&lines[index], &replacement);
            return true;
        }
    }
    false
}

fn replace_commented_in_section(
    lines: &mut [String],
    section: &str,
    key: &str,
    value: &str,
) -> bool {
    let Some((start, end)) = active_section_range(lines, section) else {
        return false;
    };
    for index in start + 1..end {
        if commented_assignment_key(&lines[index]) == Some(key) {
            let body = content(&lines[index]);
            let leading_len = body.len() - body.trim_start().len();
            let leading = &body[..leading_len];
            let replacement = format!("{leading}{key} = {value}");
            lines[index] = line_with_newline(&lines[index], &replacement);
            return true;
        }
    }
    false
}

fn insert_in_section(lines: &mut Vec<String>, section: &str, key: &str, value: &str) -> bool {
    let Some((start, mut end)) = active_section_range(lines, section) else {
        return false;
    };
    while end > start + 1 && content(&lines[end - 1]).trim().is_empty() {
        end -= 1;
    }
    while end > start + 1 && is_separator_comment(&lines[end - 1]) {
        end -= 1;
        while end > start + 1 && content(&lines[end - 1]).trim().is_empty() {
            end -= 1;
        }
    }
    let line = format!("{key} = {value}\n");
    lines.insert(end, line);
    true
}

/// `section` 内の `key` を更新する。コメント化されたキー、欠落したキー、
/// コメント化されたセクションにも対応し、テンプレートの他の行は保持する。
pub fn upsert_key(toml: &str, section: &str, key: &str, value: &TomlValue) -> String {
    let rendered = value.render();
    let mut lines = toml
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !toml.is_empty() && !toml.ends_with('\n') {
        if let Some(last) = lines.last_mut() {
            last.push('\n');
        }
    }

    let mut has_section = active_section_range(&lines, section).is_some();
    if !has_section && activate_commented_section(&mut lines, section) {
        has_section = true;
    }

    if has_section {
        if replace_in_section(&mut lines, section, key, &rendered)
            || replace_commented_in_section(&mut lines, section, key, &rendered)
            || insert_in_section(&mut lines, section, key, &rendered)
        {
            return lines.concat();
        }
    }

    let mut output = lines.concat();
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(&format!("[{section}]\n{key} = {rendered}\n"));
    output
}

/// `section` 内の有効な `key` をコメント化する。
pub fn remove_key(toml: &str, section: &str, key: &str) -> String {
    let mut lines = toml
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let Some((start, end)) = active_section_range(&lines, section) else {
        return toml.to_owned();
    };
    for index in start + 1..end {
        if assignment_key(&lines[index]) == Some(key) {
            let body = content(&lines[index]);
            let leading_len = body.len() - body.trim_start().len();
            let leading = &body[..leading_len];
            let value = body[leading_len..].trim_start();
            let replacement = format!("{leading}# {value}");
            lines[index] = line_with_newline(&lines[index], &replacement);
            break;
        }
    }
    lines.concat()
}

/// TOMLとして解釈できることを確認する。
pub fn validate(toml: &str) -> Result<(), String> {
    toml::from_str::<toml::Value>(toml)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_active_key_and_preserves_indent() {
        let input = "[server]\n  listen = \"old\"\n[other]\nvalue = true\n";
        let output = upsert_key(input, "server", "listen", &TomlValue::Str("new".into()));
        assert_eq!(
            output,
            "[server]\n  listen = \"new\"\n[other]\nvalue = true\n"
        );
    }

    #[test]
    fn replaces_active_key_and_preserves_inline_comment() {
        let input = "[server]\nlisten = \"old\" # user note\n";
        let output = upsert_key(input, "server", "listen", &TomlValue::Str("new".into()));
        assert_eq!(output, "[server]\nlisten = \"new\" # user note\n");
    }

    #[test]
    fn activates_commented_key() {
        let input = "[server]\n#   listen = \"old\"\n";
        let output = upsert_key(input, "server", "listen", &TomlValue::Str("new".into()));
        assert_eq!(output, "[server]\nlisten = \"new\"\n");
    }

    #[test]
    fn inserts_before_trailing_comment_block() {
        let input = "[server]\nlisten = \"old\"\n\n# ---- next ----\n[other]\nvalue = true\n";
        let output = upsert_key(input, "server", "web_listen", &TomlValue::Str("new".into()));
        assert_eq!(
            output,
            "[server]\nlisten = \"old\"\nweb_listen = \"new\"\n\n# ---- next ----\n[other]\nvalue = true\n"
        );
    }

    #[test]
    fn activates_commented_section_and_inserts_key() {
        let input = "# [preview]\n# 説明\n\n[other]\nvalue = true\n";
        let output = upsert_key(
            input,
            "preview",
            "command_path",
            &TomlValue::Str("ffmpeg".into()),
        );
        assert_eq!(
            output,
            "[preview]\n# 説明\ncommand_path = \"ffmpeg\"\n\n[other]\nvalue = true\n"
        );
    }

    #[test]
    fn appends_missing_section() {
        let output = upsert_key(
            "[server]\nlisten = \"x\"\n",
            "node",
            "enabled",
            &TomlValue::Bool(true),
        );
        assert!(output.ends_with("\n[node]\nenabled = true\n"));
        assert!(validate(&output).is_ok());
    }

    #[test]
    fn escapes_windows_paths_quotes_and_controls() {
        let output = upsert_key(
            "[preview]\n",
            "preview",
            "command_path",
            &TomlValue::Str("C:\\DTV\\say\"hi\nffmpeg.exe".into()),
        );
        assert!(output.contains("C:\\\\DTV"));
        assert!(output.contains("say\\\"hi"));
        assert!(output.contains("\\nffmpeg.exe"));
        assert_eq!(
            toml::from_str::<toml::Value>(&output).unwrap()["preview"]["command_path"].as_str(),
            Some("C:\\DTV\\say\"hi\nffmpeg.exe")
        );
    }

    #[test]
    fn renders_all_value_types() {
        let output = upsert_key(
            "",
            "x",
            "a",
            &TomlValue::StrArray(vec!["a".into(), "b".into()]),
        );
        let output = upsert_key(&output, "x", "b", &TomlValue::Int(42));
        assert!(output.contains("a = [\"a\", \"b\"]"));
        assert!(output.contains("b = 42"));
        assert!(validate(&output).is_ok());
    }

    #[test]
    fn upsert_is_idempotent() {
        let input = "[server]\n# listen = \"old\"\n";
        let once = upsert_key(input, "server", "listen", &TomlValue::Str("new".into()));
        let twice = upsert_key(&once, "server", "listen", &TomlValue::Str("new".into()));
        assert_eq!(once, twice);
    }

    #[test]
    fn remove_comments_active_key() {
        let input = "[server]\nlisten = \"old\"\n";
        assert_eq!(
            remove_key(input, "server", "listen"),
            "[server]\n# listen = \"old\"\n"
        );
    }

    #[test]
    fn template_accepts_all_optional_keys() {
        let template = include_str!("../recisdb-proxy.toml.example");
        let mut output = template.to_owned();
        let values = [
            ("server", "listen", TomlValue::Str("0.0.0.0:40070".into())),
            (
                "server",
                "web_listen",
                TomlValue::Str("0.0.0.0:40080".into()),
            ),
            (
                "database",
                "path",
                TomlValue::Str(r#"C:\DTV\recisdb.db"#.into()),
            ),
            ("mirakurun", "enabled", TomlValue::Bool(true)),
            ("mirakurun", "home_region", TomlValue::Str("東京".into())),
            ("node", "display_name", TomlValue::Str("test".into())),
            (
                "tsreplace",
                "command_path",
                TomlValue::Str("tsreplace".into()),
            ),
            (
                "tsreplace",
                "preprocessor_path",
                TomlValue::Str("tsreadex".into()),
            ),
            ("preview", "command_path", TomlValue::Str("ffmpeg".into())),
            (
                "preview",
                "preprocessor_path",
                TomlValue::Str("tsreadex".into()),
            ),
        ];
        for (section, key, value) in values {
            output = upsert_key(&output, section, key, &value);
        }
        validate(&output).unwrap();
    }
}
