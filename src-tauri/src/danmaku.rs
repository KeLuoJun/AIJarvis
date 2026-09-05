const MIN_MESSAGES: usize = 3;
const MAX_MESSAGES: usize = 5;
const MAX_MESSAGE_CHARS: usize = 32;

pub(crate) fn normalize_game_message(message: &str) -> Option<String> {
    let line_messages = message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    if message.contains(['\r', '\n']) && line_messages.len() >= MIN_MESSAGES {
        return normalize_messages(line_messages);
    }

    let sentence_messages = split_sentences(message);
    normalize_messages(sentence_messages)
}

fn normalize_messages(messages: Vec<&str>) -> Option<String> {
    let messages = messages.into_iter().take(MAX_MESSAGES).collect::<Vec<_>>();
    if messages.len() < MIN_MESSAGES
        || messages
            .iter()
            .any(|message| message.chars().count() > MAX_MESSAGE_CHARS)
    {
        return None;
    }
    Some(messages.join("\n"))
}

fn split_sentences(message: &str) -> Vec<&str> {
    let mut messages = Vec::new();
    let mut start = 0;
    for (index, character) in message.char_indices() {
        if matches!(character, '。' | '？' | '！') {
            let end = index + character.len_utf8();
            let sentence = message[start..end].trim();
            if !sentence.is_empty() {
                messages.push(sentence);
            }
            start = end;
        }
    }
    let tail = message[start..].trim();
    if !tail.is_empty() {
        messages.push(tail);
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_three_to_five_lines_and_drops_only_overflow() {
        assert_eq!(
            normalize_game_message(" 第一条 \r\n第二条\n\n第三条 "),
            Some("第一条\n第二条\n第三条".into())
        );
        assert_eq!(
            normalize_game_message("一\n二\n三\n四\n五\n六"),
            Some("一\n二\n三\n四\n五".into())
        );
    }

    #[test]
    fn falls_back_to_chinese_sentence_boundaries() {
        assert_eq!(
            normalize_game_message("漂亮！差一点就翻车了？稳住节奏。"),
            Some("漂亮！\n差一点就翻车了？\n稳住节奏。".into())
        );
        assert_eq!(
            normalize_game_message("漂亮！\n差一点？稳住。"),
            Some("漂亮！\n差一点？\n稳住。".into())
        );
    }

    #[test]
    fn rejects_short_or_oversized_batches_as_a_whole() {
        assert_eq!(normalize_game_message("只有一条"), None);
        assert_eq!(normalize_game_message("一\n二"), None);
        assert_eq!(
            normalize_game_message(&format!("{}\n二\n三", "长".repeat(33))),
            None
        );
    }
}
