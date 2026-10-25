use std::path::{Path, PathBuf};

pub fn filename_filter(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\\' | '/' => ' ',
            ':' => '：',
            '*' => '⭐',
            '?' => '？',
            '"' => '\'',
            '<' => '《',
            '>' => '》',
            '|' => '丨',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn temp_download_dir(
    download_root: &Path,
    provider: &str,
    comic_id: i64,
    title: &str,
) -> PathBuf {
    download_root.join(format!(
        ".下载中-{}",
        build_download_dir_name(provider, comic_id, title)
    ))
}

pub fn build_download_dir_name(provider: &str, comic_id: i64, title: &str) -> String {
    const MAX_NAME_BYTES: usize = 180;

    let sanitized = filename_filter(title);
    let (name, suffix) = if provider == "jmcomic" {
        let suffix = format!(" [JM{comic_id}]");
        let name = if sanitized.is_empty() {
            format!("JMComic {comic_id}")
        } else {
            format!("{sanitized}{suffix}")
        };
        (name, suffix)
    } else {
        let name = if sanitized.is_empty() {
            comic_id.to_string()
        } else {
            sanitized
        };
        (name, format!(" [{comic_id}]"))
    };

    if name.len() <= MAX_NAME_BYTES {
        return name;
    }

    let available = MAX_NAME_BYTES.saturating_sub(suffix.len());
    format!("{}{suffix}", truncate_utf8_by_bytes(&name, available))
}

fn truncate_utf8_by_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut end = 0;
    for (idx, ch) in input.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    input[..end].trim_end().to_string()
}

pub fn md5_hex(data: &str) -> String {
    use md5::{Digest, Md5};

    let mut hasher = Md5::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}
