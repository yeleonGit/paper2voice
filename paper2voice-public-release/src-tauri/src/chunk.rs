pub fn chunk_text(cleaned: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();

    for paragraph in cleaned
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if paragraph.chars().count() <= max_chars {
            push_or_accumulate(&mut chunks, &mut current, paragraph, max_chars);
            continue;
        }

        flush_current(&mut chunks, &mut current);

        for sentence in split_sentences(paragraph) {
            if sentence.chars().count() <= max_chars {
                push_or_accumulate(&mut chunks, &mut current, &sentence, max_chars);
            } else {
                flush_current(&mut chunks, &mut current);
                chunks.extend(hard_split(&sentence, max_chars));
            }
        }
    }

    flush_current(&mut chunks, &mut current);
    chunks
}

fn push_or_accumulate(
    chunks: &mut Vec<String>,
    current: &mut String,
    text: &str,
    max_chars: usize,
) {
    let separator_len = if current.is_empty() { 0 } else { 2 };
    let next_len = current.chars().count() + separator_len + text.chars().count();

    if next_len <= max_chars {
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(text.trim());
    } else {
        flush_current(chunks, current);
        current.push_str(text.trim());
    }
}

fn flush_current(chunks: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        chunks.push(trimmed.to_string());
    }
    current.clear();
}

fn split_sentences(paragraph: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut chars = paragraph.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }

        let next_is_boundary = chars
            .peek()
            .map(|(_, next)| next.is_whitespace())
            .unwrap_or(true);

        if next_is_boundary {
            let end = index + ch.len_utf8();
            let sentence = paragraph[start..end].trim();
            if !sentence.is_empty() {
                sentences.push(sentence.to_string());
            }

            start = end;
            while let Some((next_index, next_ch)) = chars.peek().copied() {
                if next_ch.is_whitespace() {
                    chars.next();
                    start = next_index + next_ch.len_utf8();
                } else {
                    start = next_index;
                    break;
                }
            }
        }
    }

    let tail = paragraph[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail.to_string());
    }

    sentences
}

fn hard_split(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let separator_len = if current.is_empty() { 0 } else { 1 };
        let next_len = current.chars().count() + separator_len + word.chars().count();

        if next_len <= max_chars {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        } else {
            flush_current(&mut chunks, &mut current);
            current.push_str(word);
        }
    }

    flush_current(&mut chunks, &mut current);
    chunks
}

#[cfg(test)]
mod tests {
    use super::chunk_text;

    #[test]
    fn accumulates_small_paragraphs() {
        assert_eq!(chunk_text("One.\n\nTwo.", 20), vec!["One.\n\nTwo."]);
    }
}
