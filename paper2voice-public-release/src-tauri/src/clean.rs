use regex::Regex;

pub fn clean_text(raw: &str) -> String {
    let mut text = raw.replace("\r\n", "\n").replace('\r', "\n");

    let hyphen_break = Regex::new(r"([[:alpha:]])-\n([[:alpha:]])").expect("valid regex");
    text = hyphen_break.replace_all(&text, "$1$2").to_string();

    let space_re = Regex::new(r"[ \t]+").expect("valid regex");
    let page_number_re = Regex::new(r"^\s*\d+\s*$").expect("valid regex");

    let mut paragraphs = Vec::new();
    let mut current_lines = Vec::new();

    for line in text.lines() {
        let collapsed = space_re.replace_all(line.trim(), " ");

        if collapsed.is_empty() {
            if !current_lines.is_empty() {
                paragraphs.push(current_lines.join(" "));
                current_lines.clear();
            }
            continue;
        }

        if page_number_re.is_match(&collapsed) {
            continue;
        }

        current_lines.push(collapsed.to_string());
    }

    if !current_lines.is_empty() {
        paragraphs.push(current_lines.join(" "));
    }

    paragraphs
        .into_iter()
        .map(|paragraph| space_re.replace_all(paragraph.trim(), " ").to_string())
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::clean_text;

    #[test]
    fn fixes_hyphenated_line_breaks() {
        assert_eq!(clean_text("indus-\ntrial policy"), "industrial policy");
    }

    #[test]
    fn joins_broken_lines_and_preserves_paragraphs() {
        assert_eq!(
            clean_text("This is a sentence\nthat continues here.\n\nNext paragraph."),
            "This is a sentence that continues here.\n\nNext paragraph."
        );
    }
}
